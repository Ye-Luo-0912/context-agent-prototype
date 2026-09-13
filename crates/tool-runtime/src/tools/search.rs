//! `search.grep` — rg-style regex search over workspace files.
//!
//! Model-facing output is bounded (a capped number of `file:line` hits);
//! the full hit list goes to an artifact when it overflows. Ignored
//! directories (`.git`, `.focus-agent`, `target`, `node_modules`, ...) are
//! skipped by default so build artifacts never pollute the working set.
//!
//! A bounded scan leaves two different remainders, and they have two
//! different handles (F4):
//!
//! - `cursor` pages the hits this scan *already found* out of its immutable
//!   snapshot artifact. Exhausting it exhausts the saved hits, and says
//!   nothing about whether the query itself was exhausted.
//! - `scan_continuation` resumes the *scan*: files the batch never listed
//!   and later matches in a file it stopped inside. It is a runtime-issued
//!   sealed reference to the scan position, bound to the original query, so
//!   the model can only continue a scan the runtime actually left open.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolFailureClass, ToolOutcome, ToolOutput,
    ToolRisk, ToolSemanticRole, ToolSpec, attach_failure_class,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use tokio::fs;

use super::{
    Tool, content_digest, coverage_footer, display_relative, hidden_path_output,
    is_not_found_error, missing_path_output, ordinary_view_blocked, with_coverage_footer,
};

const MAX_FILES_SCANNED: usize = 5_000;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MODEL_HITS: usize = 100;
const MAX_HIT_LINE_BYTES: usize = 1024;
/// Wire version of the sealed scan-state artifact a continuation points at.
const SCAN_STATE_VERSION: u32 = 1;

fn hit_excerpt(line: &str, match_start: usize) -> (String, bool) {
    if line.len() <= MAX_HIT_LINE_BYTES {
        return (line.trim_end().to_owned(), false);
    }
    let mut start = match_start.saturating_sub(MAX_HIT_LINE_BYTES / 4);
    while !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + MAX_HIT_LINE_BYTES).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!(
            "{}{} [line clipped; use fs.read]",
            if start == 0 { "" } else { "... " },
            &line[start..end]
        ),
        true,
    )
}
/// 大文件内每隔这么多行检查一次取消，避免整份 2 MiB 扫完才停。
const CANCEL_CHECK_LINES: usize = 256;

pub struct SearchGrepTool {
    workspace: Workspace,
    /// File candidates one batch may scan. A continuation batch gets the
    /// same budget again, so continuing never turns into an unbounded scan.
    files_per_batch: usize,
}

impl SearchGrepTool {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            files_per_batch: MAX_FILES_SCANNED,
        }
    }

    /// Shrink the per-batch file budget so tests can cross the
    /// file-candidate limit without materializing thousands of files.
    #[cfg(test)]
    fn with_files_per_batch(workspace: Workspace, files_per_batch: usize) -> Self {
        Self {
            workspace,
            files_per_batch,
        }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Opaque paging token returned by a previous `search.grep` call. When
    /// present, the next page is served from that call's snapshot artifact
    /// instead of a fresh scan, so paging stays consistent even if files
    /// change between pages.
    #[serde(default)]
    /// Parser-only compatibility. Model-visible paging uses artifact.read.
    /// This is a *result page* cursor: it re-serves hits an earlier call
    /// already found and never resumes scanning.
    cursor: Option<String>,
    /// Runtime-issued handle that resumes an unfinished scan (F4). It is
    /// the sealed reference of that call's scan-state artifact, quoted
    /// verbatim from the coverage footer — the model cannot invent one, and
    /// unlike `cursor` it continues scanning unscanned files and later
    /// matches in a partially scanned file.
    #[serde(default)]
    scan_continuation: Option<String>,
}

/// Where an unfinished scan stopped, plus the query it is bound to.
///
/// Persisted to a run-owned sealed artifact; the handle the model receives
/// is that artifact's content-addressed reference, so resuming
/// authenticates the run and the sealed digest before any of this is
/// trusted. Candidates are enumerated in ascending workspace-relative path
/// order, so a single watermark (`after_path`) plus an optional in-file
/// offset (`partial`) is a complete scan position.
#[derive(Serialize, Deserialize)]
struct ScanState {
    version: u32,
    /// The regex this continuation belongs to. A different pattern is a
    /// different query and must start its own scan.
    pattern: String,
    /// The scan root this continuation belongs to.
    path: String,
    /// Last candidate that was handled (scanned or skipped). The next batch
    /// enumerates strictly after it.
    #[serde(default)]
    after_path: Option<String>,
    /// A file left mid-scan, bound to the revision it was read at.
    #[serde(default)]
    partial: Option<PartialFile>,
    hits_total: usize,
    files_read_total: usize,
    skipped_files_total: usize,
    /// Some batch stopped listing at a walk resource cap, so parts of the
    /// tree were never enumerated and continuation cannot reach them.
    #[serde(default)]
    enumeration_truncated: bool,
    batches: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct PartialFile {
    path: String,
    /// Content digest of the file when the scan stopped inside it. A
    /// different digest invalidates the continuation instead of mixing
    /// matches from two versions.
    revision: String,
    next_line: usize,
}

fn default_path() -> String {
    String::new()
}

fn default_limit() -> usize {
    200
}

/// 扫描中途取消时保留已命中行。必须走 `Ok(Value)`：内核把工具 `Err`
/// 收成无 hits 的 `tool_error_output`，`Err(Cancelled)` 会丢掉部分结果。
fn cancelled_outcome(
    call_id: &str,
    pattern: &str,
    hits: Vec<String>,
    scanned_files: usize,
) -> ToolOutcome {
    let model_hits: Vec<String> = hits.iter().take(MODEL_HITS).cloned().collect();
    let mut metadata = json!({
        "cancelled": true,
        "hits": hits.len(),
        "files_scanned": scanned_files,
        "returned": model_hits.len(),
        "has_more": false,
        "cursor": serde_json::Value::Null,
    });
    attach_failure_class(&mut metadata, ToolFailureClass::Cancellation);
    // 有命中的取消路径与零命中一样诚实：正文必须说明扫描被打断，
    // 否则部分命中会读成完整结果（F04：positive ≠ exhaustive）。
    let model_content = if model_hits.is_empty() {
        "cancelled".into()
    } else {
        with_coverage_footer(
            model_hits.join("\n"),
            coverage_footer(vec![format!(
                "scan cancelled after {scanned_files} files; these partial hits are not the complete set"
            )]),
        )
    };
    ToolOutcome::Value(
        ToolOutput {
            call_id: call_id.into(),
            tool_name: "search.grep".into(),
            ok: false,
            summary: format!(
                "cancelled after {} hits for /{}/ across {} files",
                hits.len(),
                pattern,
                scanned_files
            ),
            model_content,
            artifact_ref: None,
            metadata,
        }
        .with_native_execution_facts(super::builtin_bound(false)),
    )
}

/// Digest prefix for model-facing revision comparisons: enough to see that
/// two revisions differ without pasting two 64-char hashes into the body.
fn short_revision(revision: &str) -> String {
    revision.chars().take(12).collect()
}

/// A continuation whose scan position no longer describes the workspace.
/// Fails closed with the two honest recoveries — restart or narrow — rather
/// than resuming across two versions of the same file.
fn invalidated_outcome(call_id: &str, pattern: &str, reason: String) -> ToolOutcome {
    let message = format!(
        "scan continuation invalidated: {reason}; restart the search for /{pattern}/ (omit scan_continuation) or narrow the path — hits already reported came from the earlier version and are not extended"
    );
    let mut metadata = json!({
        "hits": 0,
        "returned": 0,
        "has_more": false,
        "cursor": serde_json::Value::Null,
        "scan_continuation": serde_json::Value::Null,
        "scan_continuation_invalidated": true,
        "scan_complete": false,
    });
    attach_failure_class(&mut metadata, ToolFailureClass::InvalidRequest);
    ToolOutcome::Value(
        ToolOutput {
            call_id: call_id.into(),
            tool_name: "search.grep".into(),
            ok: false,
            summary: message.clone(),
            model_content: with_coverage_footer(
                "no matches from this batch".into(),
                coverage_footer(vec![message]),
            ),
            artifact_ref: None,
            metadata,
        }
        .with_native_execution_facts(super::builtin_bound(false)),
    )
}

/// One batch of candidate files in continuation order.
struct CandidateBatch {
    /// Ascending by workspace-relative path, at most one batch budget.
    files: Vec<(String, PathBuf)>,
    /// Candidates exist beyond this batch's last file.
    more_candidates: bool,
    /// Traversal stopped at a walk resource cap, so parts of the tree were
    /// never listed. Continuation cannot reach those parts.
    enumeration_truncated: bool,
}

/// Enumerate the next `budget` candidates in ascending workspace-relative
/// path order, starting strictly after `after`.
///
/// Ordering the whole scan by path is what makes a continuation possible:
/// the batch keeps only the smallest `budget` candidates above the
/// watermark (bounded memory, same as the previous first-page walk), so the
/// watermark advances monotonically and later batches reach the files an
/// earlier batch never listed.
async fn enumerate_candidates(
    workspace: &Workspace,
    root: &Path,
    after: Option<&str>,
    budget: usize,
    cancel: &CancellationToken,
) -> AgentResult<CandidateBatch> {
    // At least one candidate per batch: a zero-width batch could report
    // "more candidates" without scanning any, which is a continuation that
    // never advances.
    let budget = budget.max(1);
    let mut heap: BinaryHeap<(String, PathBuf)> = BinaryHeap::new();
    let mut more_candidates = false;
    let mut enumeration_truncated = false;
    let mut entries_left = super::MAX_WALK_ENTRIES;
    let mut path_bytes_left = super::MAX_WALK_PATH_BYTES;
    let mut entries_since_yield = 0u32;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    'walk: while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            break 'walk;
        }
        let mut reader = fs::read_dir(&dir)
            .await
            .map_err(|e| AgentError::Io(format!("read dir {}: {e}", dir.display())))?;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read dir entry: {e}")))?
        {
            if entries_left == 0 {
                enumeration_truncated = true;
                break 'walk;
            }
            entries_left -= 1;
            if cancel.is_cancelled() {
                break 'walk;
            }
            entries_since_yield += 1;
            if entries_since_yield >= super::WALK_YIELD_EVERY {
                entries_since_yield = 0;
                tokio::task::yield_now().await;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let path_bytes = path.as_os_str().as_encoded_bytes().len();
            if path_bytes > path_bytes_left {
                enumeration_truncated = true;
                break 'walk;
            }
            path_bytes_left -= path_bytes;
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| AgentError::Io(format!("file type: {e}")))?;
            if file_type.is_dir() {
                if super::is_ignored_dir(&name) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let relative = display_relative(workspace, &path);
                if after.is_some_and(|after| relative.as_str() <= after) {
                    continue;
                }
                heap.push((relative, path));
                if heap.len() > budget {
                    heap.pop();
                    more_candidates = true;
                }
            }
        }
    }

    Ok(CandidateBatch {
        files: heap.into_sorted_vec(),
        more_candidates,
        enumeration_truncated,
    })
}

/// Why a batch stopped, which is also the next scan position.
enum BatchStop {
    /// Every enumerated candidate of this batch was scanned to its end.
    Exhausted,
    /// The per-batch hit limit stopped the scan inside `partial`'s file, or
    /// right at the end of `after_path`'s file.
    HitLimit { partial: Option<PartialFile> },
}

#[async_trait]
impl Tool for SearchGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search.grep".into(),
            description: "Regex search workspace files (rg-style, bounded). Overflow returns an artifact_ref; read further lines with artifact.read. When a result reports a PARTIAL scan, its coverage line carries a scan_continuation handle: pass it back verbatim with the same pattern to keep scanning the files and lines that scan did not reach.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression"},
                    "path": {"type": "string", "description": "Optional workspace-relative file or directory: a file is searched directly, a directory is searched recursively"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::Search],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: GrepArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("search.grep args: {e}")))?;
        // 已取消则立刻交还显式 cancelled outcome，避免再付 walk/scan 成本。
        if cancel.is_cancelled() {
            return Ok(cancelled_outcome(call_id, &args.pattern, Vec::new(), 0));
        }
        // A result-page cursor re-serves hits an earlier call already found;
        // a scan continuation resumes scanning. Two handles, two meanings.
        if let Some(cursor) = args.cursor.as_deref() {
            return self.page_from_snapshot(run_id, call_id, cursor).await;
        }

        // The schema declares limit in 1..=1000; a value outside that range
        // is a shape violation, not something to silently clamp. The
        // argument parser and the schema must agree.
        if args.limit < 1 || args.limit > 1_000 {
            return Err(AgentError::InvalidRequest(format!(
                "search.grep limit must be in 1..=1000, got {}",
                args.limit
            )));
        }
        let limit = args.limit;
        let resumed = match args.scan_continuation.as_deref() {
            Some(handle) => Some(self.load_scan_state(run_id, handle, &args).await?),
            None => None,
        };
        // The continuation owns the query: a resumed batch scans the root it
        // was issued for, never a wider one a later request left blank.
        let path = match &resumed {
            Some(state) => state.path.clone(),
            None => args.path.clone(),
        };
        let regex = Regex::new(&args.pattern)
            .map_err(|e| AgentError::InvalidRequest(format!("invalid regex: {e}")))?;
        if ordinary_view_blocked(&path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id,
                "search.grep",
                &path,
            )));
        }
        let root = match self.workspace.resolve_relative(&path).await {
            Ok(root) => root,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "search.grep", &path).await,
                ));
            }
            Err(error) => return Err(error),
        };

        let after_path = resumed.as_ref().and_then(|state| state.after_path.clone());
        // `path` is grep(file-or-directory): a file is searched directly,
        // a directory recursively. (A file handed to the directory walker
        // used to surface as a confusing `directory invalid` failure.)
        let batch = match fs::metadata(&root).await {
            Ok(metadata) if metadata.is_file() => {
                let relative = display_relative(&self.workspace, &root);
                let pending = after_path
                    .as_deref()
                    .is_none_or(|after| relative.as_str() > after);
                CandidateBatch {
                    files: if pending {
                        vec![(relative, root)]
                    } else {
                        Vec::new()
                    },
                    more_candidates: false,
                    enumeration_truncated: false,
                }
            }
            Ok(_) => {
                enumerate_candidates(
                    &self.workspace,
                    &root,
                    after_path.as_deref(),
                    self.files_per_batch,
                    &cancel,
                )
                .await?
            }
            Err(_) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "search.grep", &path).await,
                ));
            }
        };
        if cancel.is_cancelled() {
            return Ok(cancelled_outcome(call_id, &args.pattern, Vec::new(), 0));
        }
        let mut hits: Vec<String> = Vec::new();
        let mut scanned_files = 0usize;
        // Files the scan could not read. A bounded coverage statement must
        // name them: partial no-hit is not repo-wide absence.
        let mut skipped_files = 0usize;
        let mut clipped_hit_lines = 0usize;
        let mut resume_in = resumed.as_ref().and_then(|state| state.partial.clone());
        // The file a continuation stopped inside must still be a candidate.
        // If it was deleted or renamed, the recorded position no longer
        // describes anything: say so instead of resuming somewhere else.
        if let Some(partial) = &resume_in {
            let still_a_candidate = batch
                .files
                .iter()
                .any(|(relative, _)| relative == &partial.path);
            if !still_a_candidate {
                return Ok(invalidated_outcome(
                    call_id,
                    &args.pattern,
                    format!("{} is no longer in the scan", partial.path),
                ));
            }
        }
        // Last candidate this batch handled (scanned or skipped) — the
        // watermark the next batch resumes after. Skipped files advance it
        // too, so a continuation always makes progress.
        let mut handled: Option<String> = after_path.clone();
        let mut stop = BatchStop::Exhausted;

        'files: for (relative, _file) in batch.files {
            if cancel.is_cancelled() {
                return Ok(cancelled_outcome(
                    call_id,
                    &args.pattern,
                    hits,
                    scanned_files,
                ));
            }
            let Some(text) =
                super::read_confined_utf8(&self.workspace, &relative, MAX_BYTES_PER_FILE).await?
            else {
                if resume_in
                    .as_ref()
                    .is_some_and(|partial| partial.path == relative)
                {
                    return Ok(invalidated_outcome(
                        call_id,
                        &args.pattern,
                        format!(
                            "{relative} is no longer readable, so the position recorded inside it cannot be resumed"
                        ),
                    ));
                }
                skipped_files += 1;
                handled = Some(relative);
                continue;
            };
            // Resuming inside a file requires the same bytes it was scanned
            // at: a changed file invalidates the continuation rather than
            // stitching matches from two revisions into one result.
            let mut start_line = 0usize;
            if resume_in
                .as_ref()
                .is_some_and(|partial| partial.path == relative)
            {
                let partial = resume_in.take().expect("presence just checked");
                let revision = content_digest(text.as_bytes());
                if revision != partial.revision {
                    return Ok(invalidated_outcome(
                        call_id,
                        &args.pattern,
                        format!(
                            "{relative} changed since it was scanned (revision {} → {})",
                            short_revision(&partial.revision),
                            short_revision(&revision)
                        ),
                    ));
                }
                start_line = partial.next_line;
            }
            scanned_files += 1;
            for (index, line) in text.lines().enumerate() {
                // 先扫一段再查 token：刚读完的文件至少能留下已匹配行。
                if index > 0 && index % CANCEL_CHECK_LINES == 0 {
                    if cancel.is_cancelled() {
                        return Ok(cancelled_outcome(
                            call_id,
                            &args.pattern,
                            hits,
                            scanned_files,
                        ));
                    }
                    tokio::task::yield_now().await;
                    if cancel.is_cancelled() {
                        return Ok(cancelled_outcome(
                            call_id,
                            &args.pattern,
                            hits,
                            scanned_files,
                        ));
                    }
                }
                if index < start_line {
                    continue;
                }
                if let Some(found) = regex.find(line) {
                    let (excerpt, clipped) = hit_excerpt(line, found.start());
                    clipped_hit_lines += usize::from(clipped);
                    hits.push(format!("{relative}:{}: {excerpt}", index + 1));
                    if hits.len() >= limit {
                        // Resume inside this file when it has later lines,
                        // otherwise after it.
                        stop = if text.lines().count() > index + 1 {
                            BatchStop::HitLimit {
                                partial: Some(PartialFile {
                                    path: relative.clone(),
                                    revision: content_digest(text.as_bytes()),
                                    next_line: index + 1,
                                }),
                            }
                        } else {
                            handled = Some(relative.clone());
                            BatchStop::HitLimit { partial: None }
                        };
                        break 'files;
                    }
                }
            }
            handled = Some(relative);
        }

        let model_hits = hits.iter().take(MODEL_HITS).cloned().collect::<Vec<_>>();
        let full = hits.join("\n");
        let artifact_ref = if hits.len() > MODEL_HITS {
            Some(
                self.workspace
                    .write_artifact(run_id, "grep", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{MODEL_HITS}")), true),
            None => (None, false),
        };

        // Coverage truth: the scan is incomplete when the hit limit stopped
        // it early, the file budget truncated the candidate list, or files
        // had to be skipped (unreadable, binary, oversized). A partial
        // no-hit must never read as repo-wide absence — and a partial hit
        // list must never read as the complete set (F04).
        let limit_reached = matches!(stop, BatchStop::HitLimit { .. });
        let partial = match stop {
            BatchStop::HitLimit { partial } => partial,
            BatchStop::Exhausted => None,
        };
        let prior = resumed.as_ref();
        let batch_number = prior.map_or(0, |state| state.batches) + 1;
        let hits_total = prior.map_or(0, |state| state.hits_total) + hits.len();
        let files_read_total = prior.map_or(0, |state| state.files_read_total) + scanned_files;
        let skipped_files_total =
            prior.map_or(0, |state| state.skipped_files_total) + skipped_files;
        let enumeration_truncated =
            prior.is_some_and(|state| state.enumeration_truncated) || batch.enumeration_truncated;
        // Work is left when the hit limit stopped this batch, or when the
        // per-batch file budget left candidates unlisted. Either way the
        // position above is enough to pick the scan back up.
        let scan_can_continue = limit_reached || batch.more_candidates;
        let scan_continuation = if scan_can_continue {
            Some(
                self.write_scan_state(
                    run_id,
                    &ScanState {
                        version: SCAN_STATE_VERSION,
                        pattern: args.pattern.clone(),
                        path: path.clone(),
                        after_path: handled.clone(),
                        partial,
                        hits_total,
                        files_read_total,
                        skipped_files_total,
                        enumeration_truncated,
                        batches: batch_number,
                    },
                )
                .await?,
            )
        } else {
            None
        };
        // "Complete" means the scan reached the end of what it could list.
        // Unlisted regions keep it false; unreadable files are declared
        // separately below.
        let scan_complete = !scan_can_continue && !enumeration_truncated;
        let scan_incomplete =
            limit_reached || batch.more_candidates || skipped_files > 0 || enumeration_truncated;

        let mut metadata = json!({
            "hits": hits.len(),
            "files_scanned": scanned_files,
            "returned": model_hits.len(),
            "has_more": has_more,
            "next_start_line": has_more.then_some(model_hits.len() + 1),
            "cursor": cursor,
            "clipped_hit_lines": clipped_hit_lines,
            "walk_budget_reached": batch.more_candidates || batch.enumeration_truncated,
            "scan_continuation": scan_continuation,
            "scan_complete": scan_complete,
            "scan_batch": batch_number,
            "hits_total": hits_total,
            "files_read_total": files_read_total,
        });
        let mut partial_reasons: Vec<String> = Vec::new();
        if limit_reached {
            partial_reasons.push("hit limit reached".into());
        }
        if batch.more_candidates {
            partial_reasons.push("file budget reached".into());
        }
        if skipped_files > 0 {
            partial_reasons.push(format!(
                "files unreadable/oversized skipped: {skipped_files}"
            ));
        }
        if enumeration_truncated {
            partial_reasons.push("directories not listed (walk budget)".into());
        }
        let coverage_note = if scan_incomplete {
            format!(
                " (PARTIAL scan: {}; do not treat this as the complete set of matches)",
                partial_reasons.join(", ")
            )
        } else {
            String::new()
        };
        metadata["scan_incomplete"] = json!(scan_incomplete);
        if skipped_files > 0 {
            metadata["skipped_files"] = json!(skipped_files);
        }
        if enumeration_truncated {
            metadata["enumeration_truncated"] = json!(true);
        }
        if skipped_files_total > skipped_files {
            metadata["skipped_files_total"] = json!(skipped_files_total);
        }
        // A no-match fact belongs to the whole query, not to one batch of a
        // continued scan that already reported hits.
        if hits_total == 0 {
            attach_failure_class(&mut metadata, ToolFailureClass::NoSearchMatch);
        }

        // The body-level coverage statement: summary/metadata never reach
        // the model, so incompleteness and the continuation pointer live
        // here, generated from the same typed facts as the summary.
        let mut clauses: Vec<String> = Vec::new();
        if batch_number > 1 {
            clauses.push(format!(
                "continued scan (batch {batch_number}): resumed after {}; earlier batches' hits are not repeated and each file is read as of the batch that scanned it",
                after_path.as_deref().unwrap_or("the start of the scan")
            ));
        }
        if scan_incomplete {
            let scope = if hits_total == 0 {
                "this is not a repo-wide absence"
            } else {
                "these are not all the matches"
            };
            clauses.push(format!(
                "PARTIAL scan: {}; {}",
                partial_reasons.join(", "),
                scope
            ));
        }
        if let Some(reference) = &artifact_ref {
            clauses.push(format!(
                "saved hits continue with artifact.read reference={reference} start_line={} (already-found hits only)",
                model_hits.len() + 1
            ));
        }
        if let Some(handle) = &scan_continuation {
            clauses.push(format!(
                "unscanned remainder continues with search.grep scan_continuation={handle} (same pattern; resumes scanning, not a result page)"
            ));
        } else if batch_number > 1 && scan_complete {
            clauses.push(format!(
                "scan complete: the continuation reached the end of this search ({hits_total} hits across {batch_number} batches); no scan continuation remains"
            ));
        } else if batch_number > 1 {
            clauses.push(format!(
                "no scan continuation remains: every listed candidate was handled ({hits_total} hits across {batch_number} batches), but the gaps named above were never scanned"
            ));
        }
        if enumeration_truncated {
            clauses.push(
                "some directories were never listed (walk budget), so continuation cannot reach them — restart with a narrower path".into(),
            );
        }
        let coverage = coverage_footer(clauses);
        let batch_note = if batch_number > 1 {
            format!(" (batch {batch_number}, {hits_total} hits total)")
        } else {
            String::new()
        };

        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "search.grep".into(),
                ok: true,
                summary: format!(
                    "{} hits for /{}/ across {} files{}{}",
                    hits.len(),
                    args.pattern,
                    scanned_files,
                    batch_note,
                    coverage_note
                ),
                model_content: with_coverage_footer(
                    if model_hits.is_empty() {
                        if batch_number > 1 {
                            "no further matches in this batch".to_string()
                        } else if scan_incomplete {
                            "no matches in the scanned files".to_string()
                        } else {
                            "no matches".to_string()
                        }
                    } else {
                        model_hits.join("\n")
                    },
                    coverage,
                ),
                artifact_ref,
                metadata,
            }
            .with_native_execution_facts(super::builtin_bound(false)),
        ))
    }
}

impl SearchGrepTool {
    /// Seal this batch's stop position into a run-owned artifact and return
    /// its reference as the continuation handle.
    async fn write_scan_state(&self, run_id: RunId, state: &ScanState) -> AgentResult<String> {
        let bytes = serde_json::to_vec(state)
            .map_err(|e| AgentError::Io(format!("serialize scan continuation: {e}")))?;
        self.workspace
            .write_artifact(run_id, "grep-scan", "json", &bytes)
            .await
    }

    /// Resolve a continuation handle back to a scan position.
    ///
    /// The handle is a sealed run-owned artifact reference, so the read
    /// authenticates the run and the content digest: an invented handle
    /// cannot resolve. The recorded query then has to match the request —
    /// a continuation is not a licence to resume a different search.
    async fn load_scan_state(
        &self,
        run_id: RunId,
        handle: &str,
        args: &GrepArgs,
    ) -> AgentResult<ScanState> {
        let bytes = super::read_snapshot_bytes(&self.workspace, run_id, handle)
            .await
            .map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "scan_continuation does not resolve to a scan this run issued: {error}; copy metadata.scan_continuation verbatim, or omit it to start a new scan"
                ))
            })?;
        let state: ScanState = serde_json::from_slice(&bytes).map_err(|e| {
            AgentError::InvalidRequest(format!(
                "scan_continuation state is unreadable: {e}; start a new scan"
            ))
        })?;
        if state.version != SCAN_STATE_VERSION {
            return Err(AgentError::InvalidRequest(format!(
                "scan_continuation was issued at state version {} (this runtime writes {SCAN_STATE_VERSION}); start a new scan",
                state.version
            )));
        }
        if state.pattern != args.pattern {
            return Err(AgentError::InvalidRequest(format!(
                "scan_continuation is bound to pattern {:?}, not {:?}; omit scan_continuation to search for a different pattern",
                state.pattern, args.pattern
            )));
        }
        if !args.path.is_empty() && args.path != state.path {
            return Err(AgentError::InvalidRequest(format!(
                "scan_continuation is bound to path {:?}, not {:?}; omit scan_continuation to search a different path",
                state.path, args.path
            )));
        }
        Ok(state)
    }

    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`); pages are capped at `MODEL_HITS` lines
    /// like the first page. Every page comes from the same immutable
    /// snapshot, so file changes between pages cannot cause duplicates or
    /// gaps.
    async fn page_from_snapshot(
        &self,
        run_id: RunId,
        call_id: &str,
        cursor: &str,
    ) -> AgentResult<ToolOutcome> {
        use super::{parse_cursor, read_snapshot_lines};

        let (reference, offset) = parse_cursor(cursor)?;
        let lines = read_snapshot_lines(&self.workspace, run_id, reference).await?;
        if offset > lines.len() {
            return Err(AgentError::InvalidRequest(format!(
                "cursor is past the end of the snapshot ({offset} > {} lines)",
                lines.len()
            )));
        }
        let page: Vec<&str> = lines
            .iter()
            .skip(offset)
            .take(MODEL_HITS)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        // Every page names its range and snapshot identity, and carries an
        // explicit continuation or end marker: one page of text must never
        // read as the whole result set (F04 分页语义). The last page only
        // ends the *saved* hit list — whether the query itself was
        // exhausted is a property of the scan that produced the snapshot,
        // and is resumed through that call's scan_continuation, not here.
        let coverage = if has_more {
            coverage_footer(vec![format!(
                "saved hits {}-{} of {} (snapshot {reference}); continue with search.grep cursor={reference}#{next_offset}",
                offset + 1,
                next_offset,
                lines.len()
            )])
        } else {
            coverage_footer(vec![format!(
                "end of saved results ({next_offset} total, snapshot {reference}): these are the hits that scan had already found, not proof the search was exhausted; if that scan reported a partial coverage, resume it with its scan_continuation"
            )])
        };

        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "search.grep".into(),
                ok: true,
                summary: format!(
                    "hit page {}-{} of {} (snapshot)",
                    offset + 1,
                    next_offset,
                    lines.len()
                ),
                model_content: with_coverage_footer(
                    if page.is_empty() {
                        "no more hits".to_string()
                    } else {
                        page.join("\n")
                    },
                    coverage,
                ),
                artifact_ref: Some(reference.to_string()),
                metadata: json!({
                    "hits": lines.len(),
                    "returned": page.len(),
                    "has_more": has_more,
                    "cursor": next_cursor,
                }),
            }
            .with_native_execution_facts(super::builtin_bound(false)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;
    use std::path::Path;
    /// Unwrap a plain tool value (search.grep never stages an effect).
    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("search.grep must return a plain value"),
        }
    }

    async fn temp_workspace() -> (Workspace, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        (workspace, dir)
    }

    #[tokio::test]
    async fn huge_matching_lines_return_bounded_excerpts_around_the_match() {
        let (workspace, dir) = temp_workspace().await;
        let line = format!("{}needle{}", "界".repeat(100_000), "x".repeat(100_000));
        std::fs::write(dir.path().join("large.txt"), &line).unwrap();
        let tool = SearchGrepTool::new(workspace);
        let output = value(
            tool.execute(
                RunId::new(),
                "long",
                json!({"pattern":"needle"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok);
        assert!(output.model_content.contains("needle"));
        assert!(output.model_content.len() < MAX_HIT_LINE_BYTES + 200);
        assert_eq!(output.metadata["clipped_hit_lines"], 1);
    }

    async fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Run one `search.grep` batch with a live cancellation token.
    async fn grep_call(tool: &SearchGrepTool, run_id: RunId, args: Value) -> ToolOutput {
        value(
            tool.execute(run_id, "c", args, None, CancellationToken::new())
                .await
                .unwrap(),
        )
    }

    /// The runtime-issued scan continuation of a batch that left work.
    fn continuation(output: &ToolOutput) -> String {
        output.metadata["scan_continuation"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "batch must issue a scan continuation: {:?}",
                    output.metadata
                )
            })
            .to_string()
    }

    /// Hit lines only: the coverage footer is machine-generated commentary,
    /// not part of the result set.
    fn hit_lines(output: &ToolOutput) -> Vec<String> {
        output
            .model_content
            .lines()
            .filter(|line| !line.starts_with("[coverage]"))
            .map(str::to_owned)
            .collect()
    }

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "search.grep".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn grep_accepts_a_single_file_path() {
        // grep(file-or-directory): an explicit file target must search that
        // one file, not fail with a directory-walker error (and not leak
        // matches from sibling files).
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/protocol.rs", "handle_auth() {}\n").await;
        write(&root, "src/other.rs", "handle_auth() leak\n").await;

        let tool = SearchGrepTool::new(workspace);
        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "handle_auth", "path": "src/protocol.rs"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok, "a file target must search the file: {output:?}");
        assert!(
            output.model_content.contains("src/protocol.rs:1"),
            "hit inside the named file: {}",
            output.model_content
        );
        assert!(
            !output.model_content.contains("other.rs"),
            "sibling files must not leak: {}",
            output.model_content
        );
        assert_eq!(output.metadata["files_scanned"], 1);
    }

    #[tokio::test]
    async fn grep_finds_matches_and_skips_ignored_dirs() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/lib.rs", "fn auth() {}\nfn main() { auth(); }\n").await;
        write(&root, "src/main.rs", "auth()\n").await;
        // Ignored: build artifacts and state dir must not be searched.
        write(&root, "target/debug/lib.rs", "auth() secret\n").await;
        write(&root, ".focus-agent/traces/x.jsonl", "auth() secret\n").await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(run_id, json!({"pattern": "auth"}));
        let output = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        let content = output.model_content;
        assert!(
            content.contains("src/lib.rs:1"),
            "hit lib.rs line 1: {content}"
        );
        assert!(content.contains("src/main.rs:1"), "hit main.rs: {content}");
        assert!(
            !content.contains("target/") && !content.contains(".focus-agent/"),
            "ignored dirs leaked into results: {content}"
        );
        assert!(!content.contains("secret"));
    }

    #[tokio::test]
    async fn grep_bounds_model_content() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!("match_{i}: something\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(run_id, json!({"pattern": "match_", "limit": 300}));
        let output = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let output = value(output);
        assert!(
            output.artifact_ref.is_some(),
            "overflow must go to an artifact"
        );
        assert!(
            output.model_content.matches("match_").count() <= MODEL_HITS,
            "model content exceeded the hit cap"
        );
    }

    #[tokio::test]
    async fn grep_pages_a_consistent_snapshot() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..250 {
            body.push_str(&format!("match_{i:03}: something\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();

        let grep = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "search.grep".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Page 1: 250 hits, 100 shown, cursor + snapshot spill.
        let first = grep(json!({"pattern": "match_", "limit": 300}))
            .await
            .unwrap();
        let first = value(first);
        assert_eq!(first.metadata["hits"], 250);
        assert_eq!(first.metadata["has_more"], true);
        let cursor = first.metadata["cursor"].as_str().unwrap().to_string();
        assert!(first.artifact_ref.is_some());

        // The source file changes between pages; paging must not notice.
        std::fs::write(root.join("big.txt"), "match_000: changed\n").unwrap();

        // Page 2 serves the next 100 snapshot hits.
        let second = grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor}))
            .await
            .unwrap();
        let second = value(second);
        assert_eq!(
            second.metadata["hits"], 250,
            "total comes from the snapshot"
        );
        assert_eq!(second.metadata["returned"], 100);
        assert!(second.metadata["has_more"].as_bool().unwrap());
        let first_lines: Vec<&str> = first.model_content.lines().collect();
        let second_lines: Vec<&str> = second.model_content.lines().collect();
        assert!(
            second_lines.iter().all(|line| !first_lines.contains(line)),
            "pages must not overlap"
        );
        assert!(
            !second_lines.iter().any(|line| line.contains("changed")),
            "the snapshot must not see later file edits"
        );

        // Drain the last 50.
        let cursor2 = second.metadata["cursor"].as_str().unwrap().to_string();
        let third = grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor2}))
            .await
            .unwrap();
        let third = value(third);
        assert_eq!(third.metadata["returned"], 50);
        assert_eq!(third.metadata["has_more"], false);
        assert!(third.metadata["cursor"].is_null());

        // A malformed cursor is a clean error.
        let bad = grep(json!({"pattern": "match_", "limit": 300, "cursor": "not-a-cursor"})).await;
        assert!(bad.is_err(), "malformed cursors must error");
    }

    #[tokio::test]
    async fn grep_honors_preexisting_cancellation() {
        let (workspace, _dir) = temp_workspace().await;
        write(workspace.root(), "src/a.rs", "needle\n").await;

        let tool = SearchGrepTool::new(workspace);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "needle"}),
                None,
                cancel,
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(!output.ok);
        assert_eq!(output.metadata["cancelled"], true);
        assert_eq!(output.metadata["hits"], 0);
        assert_eq!(output.metadata["files_scanned"], 0);
        assert_eq!(output.model_content, "cancelled");
    }

    #[tokio::test]
    async fn grep_stops_mid_scan_and_returns_partial_hits() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        // 每文件少量命中、大量非命中行：不会先撞上 1000-hit limit，
        // 取消必须靠打断 walk/scan，而不是“扫完了”。
        const FILE_COUNT: usize = 80;
        let mut body = String::from("needle unique\n");
        for _ in 0..1_200 {
            body.push_str("padding line that does not match\n");
        }
        for n in 0..FILE_COUNT {
            write(&root, &format!("src/f{n:03}.txt"), &body).await;
        }

        let tool = SearchGrepTool::new(workspace);
        let cancel = CancellationToken::new();
        let fire = cancel.clone();
        let join = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3));
            fire.cancel();
        });

        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "needle", "limit": 1_000}),
                None,
                cancel,
            )
            .await
            .unwrap();
        join.join().unwrap();
        let output = value(output);
        assert!(!output.ok, "cancelled grep must not report ok");
        assert_eq!(output.metadata["cancelled"], true);
        let scanned = output.metadata["files_scanned"].as_u64().unwrap();
        assert!(
            scanned < FILE_COUNT as u64,
            "must stop before scanning every file: scanned={scanned}"
        );
        assert!(
            output.summary.contains("cancelled"),
            "summary must name cancellation: {}",
            output.summary
        );
    }
    #[tokio::test]
    async fn grep_reports_partial_scan_when_the_hit_limit_stops_it() {
        // A hit limit that stops the walk early is a bounded coverage fact:
        // the result must not read as repo-wide absence or a complete set.
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "src/a_first.rs",
            "needle
needle
",
        )
        .await;
        write(
            &root,
            "src/z_last.rs",
            "needle
",
        )
        .await;

        let tool = SearchGrepTool::new(workspace);
        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "needle", "limit": 1}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert_eq!(
            output.metadata["scan_incomplete"],
            json!(true),
            "{:?}",
            output.metadata
        );
        assert!(
            output.summary.contains("PARTIAL scan"),
            "summary must name the partial scan: {}",
            output.summary
        );
        assert!(output.summary.contains("hit limit reached"));
    }

    #[tokio::test]
    async fn grep_complete_scan_without_hits_claims_absence_plainly() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "src/plain.rs",
            "nothing special here
",
        )
        .await;

        let tool = SearchGrepTool::new(workspace);
        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "definitely-absent-token"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert_eq!(output.metadata["scan_incomplete"], json!(false));
        assert!(!output.summary.contains("PARTIAL"), "{}", output.summary);
        assert_eq!(output.model_content, "no matches");
    }

    #[tokio::test]
    async fn grep_skips_oversized_files_through_the_confined_read_and_marks_partial() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/small.rs", "needle here\n").await;
        std::fs::write(
            root.join("src/huge.rs"),
            vec![b'x'; (MAX_BYTES_PER_FILE as usize) + 1],
        )
        .unwrap();

        let tool = SearchGrepTool::new(workspace);
        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "needle"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert!(
            output.model_content.contains("src/small.rs"),
            "{}",
            output.model_content
        );
        assert_eq!(output.metadata["scan_incomplete"], json!(true));
        assert_eq!(output.metadata["skipped_files"], json!(1));
        assert!(
            output.summary.contains("PARTIAL scan"),
            "{}",
            output.summary
        );
    }

    /// F04 反例：一个可读文件命中、另一个关键文件因超限被跳过。正文
    /// （model_content）是唯一进入 TurnFrame 的字段——有命中也不能把
    /// 不完整扫描伪装成完整命中列表；coverage 声明必须长在正文里。
    #[tokio::test]
    async fn hits_with_a_skipped_file_declare_incompleteness_in_the_model_body() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/small.rs", "needle here\n").await;
        std::fs::write(
            root.join("src/huge.rs"),
            vec![b'x'; (MAX_BYTES_PER_FILE as usize) + 1],
        )
        .unwrap();

        let tool = SearchGrepTool::new(workspace);
        let output = tool
            .execute(
                RunId::new(),
                "c",
                json!({"pattern": "needle"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert!(
            output.model_content.contains("src/small.rs:1"),
            "the hit itself must still be present: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("[coverage]") && output.model_content.contains("PARTIAL"),
            "a hit list from a partial scan must carry the coverage statement in the body: {}",
            output.model_content
        );
        assert!(
            output
                .model_content
                .contains("files unreadable/oversized skipped: 1"),
            "the body must name the skip count, not bury it in metadata: {}",
            output.model_content
        );
    }

    /// 取消留下的部分命中同样不能读成完整结果：正文必须说明扫描被
    /// 打断（零命中路径的 "cancelled" 早已诚实，这里补齐有命中路径）。
    #[test]
    fn a_cancelled_partial_hit_list_says_the_scan_stopped_in_the_body() {
        let outcome = cancelled_outcome(
            "c",
            "needle",
            vec!["src/a.rs:1: needle".into(), "src/b.rs:2: needle".into()],
            7,
        );
        let ToolOutcome::Value(output) = outcome else {
            panic!("cancelled_outcome returns a plain value");
        };
        assert!(
            output.model_content.contains("src/a.rs:1"),
            "partial hits stay visible: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("[coverage]")
                && output
                    .model_content
                    .contains("scan cancelled after 7 files"),
            "partial hits from a cancelled scan must say so in the body: {}",
            output.model_content
        );
    }

    /// 快照分页在正文里保留范围身份与结束标记：中间页给出续读指针，
    /// 最后一页明确 end of results——一页文本永远不能读成全部结果。
    #[tokio::test]
    async fn snapshot_pages_carry_identity_and_an_end_marker_in_the_body() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..220 {
            body.push_str(&format!("match_{i:03}: something\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();
        let grep = |args: Value| {
            let tool = &tool;
            async move {
                tool.execute(run_id, "c", args, None, CancellationToken::new())
                    .await
            }
        };

        let first = value(
            grep(json!({"pattern": "match_", "limit": 300}))
                .await
                .unwrap(),
        );
        let cursor = first.metadata["cursor"].as_str().unwrap().to_string();
        assert!(
            first
                .model_content
                .contains("continue with artifact.read reference="),
            "the first page must name the continuation in the body: {}",
            first.model_content
        );

        let second = value(
            grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor}))
                .await
                .unwrap(),
        );
        assert!(
            second
                .model_content
                .contains("hits 101-200 of 220 (snapshot"),
            "a middle page must carry its range and snapshot identity: {}",
            second.model_content
        );
        assert!(
            second
                .model_content
                .contains("continue with search.grep cursor="),
            "a middle page must name its continuation: {}",
            second.model_content
        );
        let cursor2 = second.metadata["cursor"].as_str().unwrap().to_string();
        let third = value(
            grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor2}))
                .await
                .unwrap(),
        );
        assert!(
            third
                .model_content
                .contains("end of saved results (220 total, snapshot"),
            "the final page must carry the end marker: {}",
            third.model_content
        );
        // F4: exhausting a snapshot only exhausts the *saved* hits. It is
        // not a statement about the query, whose remainder (if any) is
        // resumed through that scan's continuation.
        assert!(
            third
                .model_content
                .contains("not proof the search was exhausted"),
            "the last saved page must not claim the query is exhausted: {}",
            third.model_content
        );
    }

    /// F4: the hit limit stops a scan inside a file. The continuation must
    /// resume at the next line of that same file, not re-page the hits the
    /// first batch already returned.
    #[tokio::test]
    async fn a_continuation_finds_matches_after_the_hit_limit_in_one_file() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..12 {
            body.push_str(&format!("needle_{index:02}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();

        let first = grep_call(&tool, run_id, json!({"pattern": "needle_", "limit": 5})).await;
        assert_eq!(first.metadata["hits"], 5);
        assert_eq!(first.metadata["scan_complete"], json!(false));
        assert!(
            first
                .model_content
                .contains("unscanned remainder continues with search.grep scan_continuation="),
            "the body must name the runtime-issued continuation: {}",
            first.model_content
        );
        assert!(first.model_content.contains("src/one.txt:5: needle_04"));
        assert!(!first.model_content.contains("needle_05"));

        let second = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 5, "scan_continuation": continuation(&first)}),
        )
        .await;
        assert_eq!(second.metadata["hits"], 5);
        assert!(
            second.model_content.contains("src/one.txt:6: needle_05")
                && second.model_content.contains("src/one.txt:10: needle_09"),
            "the continuation must scan later lines of the same file: {}",
            second.model_content
        );
        assert!(
            !second.model_content.contains("needle_04"),
            "a continued batch must not repeat earlier hits: {}",
            second.model_content
        );
        assert_eq!(second.metadata["hits_total"], 10);

        let third = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 5, "scan_continuation": continuation(&second)}),
        )
        .await;
        assert_eq!(third.metadata["hits"], 2);
        assert_eq!(third.metadata["hits_total"], 12);
        assert_eq!(third.metadata["scan_complete"], json!(true));
        assert!(third.metadata["scan_continuation"].is_null());
        assert!(
            third.model_content.contains("scan complete"),
            "the last batch must declare completeness: {}",
            third.model_content
        );
    }

    /// F4: a directory with more files than one batch may scan. The
    /// continuation must reach the later files instead of re-serving the
    /// hits of the first batch, and every batch keeps the file budget.
    #[tokio::test]
    async fn a_continuation_reaches_files_beyond_the_file_candidate_limit() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        for index in 0..5 {
            write(&root, &format!("src/f{index}.txt"), "needle here\n").await;
        }

        let tool = SearchGrepTool::with_files_per_batch(workspace, 2);
        let run_id = RunId::new();
        let mut hits: Vec<String> = Vec::new();
        let mut handle: Option<String> = None;
        let mut batches = 0usize;

        let last = loop {
            batches += 1;
            assert!(batches <= 5, "a continuation chain must terminate");
            let mut args = json!({"pattern": "needle", "limit": 100});
            if let Some(handle) = &handle {
                args["scan_continuation"] = json!(handle);
            }
            let output = grep_call(&tool, run_id, args).await;
            assert!(
                output.metadata["files_scanned"].as_u64().unwrap() <= 2,
                "a continued batch keeps the per-batch file budget: {:?}",
                output.metadata
            );
            hits.extend(hit_lines(&output));
            match output.metadata["scan_continuation"].as_str() {
                Some(next) => handle = Some(next.to_owned()),
                None => break output,
            }
        };

        assert_eq!(batches, 3, "2 files per batch over 5 files");
        assert_eq!(last.metadata["scan_complete"], json!(true));
        assert!(
            last.model_content.contains("scan complete"),
            "the final batch must declare the scan finished: {}",
            last.model_content
        );
        let mut files: Vec<&str> = hits
            .iter()
            .map(|line| line.split(':').next().unwrap())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "src/f0.txt",
                "src/f1.txt",
                "src/f2.txt",
                "src/f3.txt",
                "src/f4.txt"
            ],
            "continuation must reach every file exactly once: {hits:?}"
        );
    }

    /// F4: the two handles are different capabilities. `cursor` re-serves
    /// hits the scan already found; `scan_continuation` resumes scanning.
    #[tokio::test]
    async fn a_result_page_cursor_and_a_scan_continuation_stay_distinct() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..250 {
            body.push_str(&format!("match_{index:03}\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(&tool, run_id, json!({"pattern": "match_", "limit": 150})).await;
        assert_eq!(first.metadata["hits"], 150);
        let cursor = first.metadata["cursor"].as_str().unwrap().to_owned();
        let handle = continuation(&first);
        assert_ne!(cursor, handle, "paging and scanning are separate handles");
        assert!(
            first
                .model_content
                .contains("saved hits continue with artifact.read reference="),
            "{}",
            first.model_content
        );
        assert!(
            first
                .model_content
                .contains("resumes scanning, not a result page"),
            "the body must separate the two continuations: {}",
            first.model_content
        );

        // The result page stays inside the saved snapshot.
        let page = grep_call(
            &tool,
            run_id,
            json!({"pattern": "match_", "limit": 150, "cursor": cursor}),
        )
        .await;
        assert_eq!(page.metadata["hits"], 150, "the snapshot holds 150 hits");
        assert!(page.model_content.contains("match_100"));
        assert!(
            page.metadata["scan_continuation"].is_null(),
            "paging saved hits must not mint a scan position"
        );

        // The scan continuation reaches matches the snapshot never held.
        let resumed = grep_call(
            &tool,
            run_id,
            json!({"pattern": "match_", "limit": 150, "scan_continuation": handle}),
        )
        .await;
        assert!(
            resumed.model_content.contains("match_150"),
            "the continuation must scan past the saved hits: {}",
            resumed.model_content
        );
    }

    /// F4: a file that changed under a recorded position invalidates the
    /// continuation, and the refusal names the two honest recoveries. Hits
    /// from the new revision are never stitched onto the old ones.
    #[tokio::test]
    async fn a_changed_file_invalidates_the_continuation_instead_of_mixing_versions() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..10 {
            body.push_str(&format!("needle_{index}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(&tool, run_id, json!({"pattern": "needle_", "limit": 3})).await;
        let handle = continuation(&first);

        write(&root, "src/one.txt", "needle_rewritten\n").await;

        let resumed = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 3, "scan_continuation": handle}),
        )
        .await;
        assert!(
            !resumed.ok,
            "an invalidated continuation must not report ok"
        );
        assert_eq!(
            resumed.metadata["scan_continuation_invalidated"],
            json!(true)
        );
        assert_eq!(resumed.metadata["hits"], 0);
        assert!(
            resumed
                .model_content
                .contains("changed since it was scanned")
                && resumed.model_content.contains("restart the search")
                && resumed.model_content.contains("narrow the path"),
            "invalidation must say restart or narrow: {}",
            resumed.model_content
        );
        assert!(
            !resumed.model_content.contains("needle_rewritten"),
            "no hit from the new revision may be mixed in: {}",
            resumed.model_content
        );
    }

    /// F4: the same rule for a directory change that removes the file a
    /// continuation stopped inside.
    #[tokio::test]
    async fn a_removed_file_invalidates_the_recorded_scan_position() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..10 {
            body.push_str(&format!("needle_{index}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(&tool, run_id, json!({"pattern": "needle_", "limit": 3})).await;
        let handle = continuation(&first);

        std::fs::remove_file(root.join("src/one.txt")).unwrap();

        let resumed = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 3, "scan_continuation": handle}),
        )
        .await;
        assert!(!resumed.ok);
        assert!(
            resumed.model_content.contains("no longer in the scan")
                && resumed.model_content.contains("restart the search"),
            "a vanished position must invalidate, not silently skip: {}",
            resumed.model_content
        );
    }

    /// F4: continuations are positions, not tickets. Replaying one returns
    /// the same batch instead of skipping over unscanned content.
    #[tokio::test]
    async fn replaying_a_continuation_returns_the_same_batch() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..12 {
            body.push_str(&format!("needle_{index:02}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(&tool, run_id, json!({"pattern": "needle_", "limit": 4})).await;
        let handle = continuation(&first);

        let once = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 4, "scan_continuation": handle.clone()}),
        )
        .await;
        let twice = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 4, "scan_continuation": handle}),
        )
        .await;
        assert_eq!(once.model_content, twice.model_content);
        assert_eq!(once.metadata["hits"], twice.metadata["hits"]);
        assert_eq!(
            once.metadata["scan_continuation"], twice.metadata["scan_continuation"],
            "the same position must hand back the same next position"
        );
    }

    /// F4: a continued batch is cancellable like the first one, and a
    /// cancelled batch neither consumes the position nor loses prior hits.
    #[tokio::test]
    async fn a_cancelled_continuation_batch_keeps_its_position_and_prior_hits() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..9 {
            body.push_str(&format!("needle_{index}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(&tool, run_id, json!({"pattern": "needle_", "limit": 3})).await;
        let handle = continuation(&first);

        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = value(
            tool.execute(
                run_id,
                "c",
                json!({"pattern": "needle_", "limit": 3, "scan_continuation": handle.clone()}),
                None,
                cancel,
            )
            .await
            .unwrap(),
        );
        assert!(!cancelled.ok);
        assert_eq!(cancelled.metadata["cancelled"], json!(true));

        let resumed = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 3, "scan_continuation": handle}),
        )
        .await;
        assert_eq!(
            resumed.metadata["hits"], 3,
            "a cancelled batch must leave the position usable"
        );
        assert!(resumed.model_content.contains("src/one.txt:4: needle_3"));
        assert_eq!(resumed.metadata["hits_total"], 6);
    }

    /// F4: the handle is runtime-issued and query-bound. A model cannot
    /// invent one, and it cannot repoint an existing one at another search.
    #[tokio::test]
    async fn a_continuation_is_bound_to_the_query_that_issued_it() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for index in 0..8 {
            body.push_str(&format!("needle_{index}\n"));
        }
        write(&root, "src/one.txt", &body).await;

        let tool = SearchGrepTool::new(workspace);
        let run_id = RunId::new();
        let first = grep_call(
            &tool,
            run_id,
            json!({"pattern": "needle_", "limit": 2, "path": "src"}),
        )
        .await;
        let handle = continuation(&first);

        let other_pattern = tool
            .execute(
                run_id,
                "c",
                json!({"pattern": "other", "limit": 2, "scan_continuation": handle.clone()}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            other_pattern.to_string().contains("bound to pattern"),
            "{other_pattern}"
        );

        let other_path = tool
            .execute(
                run_id,
                "c",
                json!({"pattern": "needle_", "limit": 2, "path": "", "scan_continuation": handle}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(
            other_path.is_ok(),
            "an omitted path keeps the continuation's own root"
        );

        let invented = tool
            .execute(
                run_id,
                "c",
                json!({
                    "pattern": "needle_",
                    "limit": 2,
                    "scan_continuation": "artifact://v1/00000000-0000-4000-8000-000000000000/grep-scan/0123456789abcdef"
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            invented.to_string().contains("does not resolve"),
            "an invented continuation must fail closed: {invented}"
        );
    }

    /// A scan that covered everything issues no continuation, so a complete
    /// result never carries a resume pointer the model could misread.
    #[tokio::test]
    async fn a_complete_scan_issues_no_continuation() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/a.rs", "needle here\n").await;
        write(&root, "src/b.rs", "needle there\n").await;

        let tool = SearchGrepTool::new(workspace);
        let output = grep_call(&tool, RunId::new(), json!({"pattern": "needle"})).await;
        assert_eq!(output.metadata["hits"], 2);
        assert_eq!(output.metadata["scan_complete"], json!(true));
        assert!(output.metadata["scan_continuation"].is_null());
        assert!(
            !output.model_content.contains("[coverage]"),
            "a complete scan stays plain: {}",
            output.model_content
        );
    }
}

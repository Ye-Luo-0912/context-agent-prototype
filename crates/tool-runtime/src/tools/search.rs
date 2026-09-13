//! `search.grep` — rg-style regex search over workspace files.
//!
//! Model-facing output is bounded (a capped number of `file:line` hits);
//! the full hit list goes to an artifact when it overflows. Ignored
//! directories (`.git`, `.focus-agent`, `target`, `node_modules`, ...) are
//! skipped by default so build artifacts never pollute the working set.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolFailureClass, ToolOutcome, ToolOutput,
    ToolRisk, ToolSemanticRole, ToolSpec, attach_failure_class,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::{
    Tool, coverage_footer, display_relative, hidden_path_output, is_not_found_error,
    missing_path_output, ordinary_view_blocked, walk_files, with_coverage_footer,
};

const MAX_FILES_SCANNED: usize = 5_000;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MODEL_HITS: usize = 100;
const MAX_HIT_LINE_BYTES: usize = 1024;

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
}

impl SearchGrepTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
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
    cursor: Option<String>,
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

#[async_trait]
impl Tool for SearchGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search.grep".into(),
            description: "Regex search workspace files (rg-style, bounded). Overflow returns an artifact_ref; read further lines with artifact.read.".into(),
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
        if let Some(cursor) = args.cursor.as_deref() {
            return self.page_from_snapshot(run_id, call_id, cursor).await;
        }
        let regex = Regex::new(&args.pattern)
            .map_err(|e| AgentError::InvalidRequest(format!("invalid regex: {e}")))?;
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id,
                "search.grep",
                &args.path,
            )));
        }
        let root = match self.workspace.resolve_relative(&args.path).await {
            Ok(root) => root,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "search.grep", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };

        // `path` is grep(file-or-directory): a file is searched directly,
        // a directory recursively. (A file handed to the directory walker
        // used to surface as a confusing `directory invalid` failure.)
        let mut files = Vec::new();
        let mut walk_budget_reached = false;
        match fs::metadata(&root).await {
            Ok(metadata) if metadata.is_file() => files.push(root),
            Ok(_) => {
                let mut budget = MAX_FILES_SCANNED;
                walk_budget_reached =
                    walk_files(&root, &mut files, &mut budget, Some(&cancel)).await?;
            }
            Err(_) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "search.grep", &args.path).await,
                ));
            }
        }
        if cancel.is_cancelled() {
            return Ok(cancelled_outcome(call_id, &args.pattern, Vec::new(), 0));
        }
        files.sort();

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
        let mut hits: Vec<String> = Vec::new();
        let mut scanned_files = 0usize;
        // Files the scan could not read. A bounded coverage statement must
        // name them: partial no-hit is not repo-wide absence.
        let mut skipped_files = 0usize;
        let mut clipped_hit_lines = 0usize;

        'files: for file in files {
            if cancel.is_cancelled() {
                return Ok(cancelled_outcome(
                    call_id,
                    &args.pattern,
                    hits,
                    scanned_files,
                ));
            }
            let relative = display_relative(&self.workspace, &file);
            let Some(text) =
                super::read_confined_utf8(&self.workspace, &relative, MAX_BYTES_PER_FILE).await?
            else {
                skipped_files += 1;
                continue;
            };
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
                if let Some(found) = regex.find(line) {
                    let (excerpt, clipped) = hit_excerpt(line, found.start());
                    clipped_hit_lines += usize::from(clipped);
                    hits.push(format!("{relative}:{}: {excerpt}", index + 1));
                    if hits.len() >= limit {
                        break 'files;
                    }
                }
            }
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

        let mut metadata = json!({
            "hits": hits.len(),
            "files_scanned": scanned_files,
            "returned": model_hits.len(),
            "has_more": has_more,
            "next_start_line": has_more.then_some(model_hits.len() + 1),
            "cursor": cursor,
            "clipped_hit_lines": clipped_hit_lines,
            "walk_budget_reached": walk_budget_reached,
        });
        // Coverage truth: the scan is incomplete when the hit limit stopped
        // it early, the file budget truncated the candidate list, or files
        // had to be skipped (unreadable, binary, oversized). A partial
        // no-hit must never read as repo-wide absence — and a partial hit
        // list must never read as the complete set (F04).
        let limit_reached = hits.len() >= limit;
        let scan_incomplete = limit_reached || walk_budget_reached || skipped_files > 0;
        let mut partial_reasons: Vec<String> = Vec::new();
        if limit_reached {
            partial_reasons.push("hit limit reached".into());
        }
        if walk_budget_reached {
            partial_reasons.push("file budget reached".into());
        }
        if skipped_files > 0 {
            partial_reasons.push(format!(
                "files unreadable/oversized skipped: {skipped_files}"
            ));
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
        if hits.is_empty() {
            attach_failure_class(&mut metadata, ToolFailureClass::NoSearchMatch);
        }

        // The body-level coverage statement: summary/metadata never reach
        // the model, so incompleteness and the continuation pointer live
        // here, generated from the same typed facts as the summary.
        let mut clauses: Vec<String> = Vec::new();
        if scan_incomplete {
            let scope = if model_hits.is_empty() {
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
                "continue with artifact.read reference={reference} start_line={}",
                model_hits.len() + 1
            ));
        }
        let coverage = coverage_footer(clauses);

        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "search.grep".into(),
                ok: true,
                summary: format!(
                    "{} hits for /{}/ across {} files{}",
                    hits.len(),
                    args.pattern,
                    scanned_files,
                    coverage_note
                ),
                model_content: with_coverage_footer(
                    if model_hits.is_empty() {
                        if scan_incomplete {
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
        // read as the whole result set (F04 分页语义).
        let coverage = if has_more {
            coverage_footer(vec![format!(
                "hits {}-{} of {} (snapshot {reference}); continue with search.grep cursor={reference}#{next_offset}",
                offset + 1,
                next_offset,
                lines.len()
            )])
        } else {
            coverage_footer(vec![format!(
                "end of results ({next_offset} total, snapshot {reference})"
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
                .contains("end of results (220 total, snapshot"),
            "the final page must carry the end marker: {}",
            third.model_content
        );
    }
}

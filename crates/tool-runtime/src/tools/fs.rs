use std::path::Path;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolExecutionFacts, ToolOutcome,
    ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
};
use agent_workspace::{DirectoryCreationPreparation, MAX_MUTATION_BYTES, Workspace};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::page::{
    DeliveredSpan, FINAL_BODY_CHARS, finalize_within_budget, render_numbered_spans,
    rendered_prefix_chars,
};
use super::{
    LineEnding, Tool, content_digest, coverage_footer, hidden_path_output, is_hidden_name,
    is_not_found_error, missing_parent_output, missing_path_output, model_json_string,
    ordinary_view_blocked, with_coverage_footer,
};

// A revision returned by `fs.read` must be usable by the canonical edit
// tools for every file the workspace mutation layer admits.
const MAX_READ_BYTES: u64 = MAX_MUTATION_BYTES as u64;
const MAX_WRITE_BYTES: usize = MAX_MUTATION_BYTES;
const MAX_READ_LINES: usize = 400;
const MAX_LIST_ENTRIES: usize = 2_000;
/// The page size `fs.read` continues with past the requested window (the
/// same size its own `end_line` default yields for the default start):
/// a file-level continuation walks this many lines at a time.
const FS_READ_PAGE_LINES: usize = 200;

pub struct FsListTool {
    workspace: Workspace,
}

impl FsListTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
    /// Opaque paging token returned by a previous `fs.list` call. When
    /// present, the next page is served from that call's snapshot artifact
    /// instead of a fresh directory scan, so paging stays consistent even
    /// if the directory changes between pages.
    #[serde(default)]
    /// Parser-only compatibility for non-model callers. Model-visible
    /// continuation is centralized on `artifact.read` so an opaque token is
    /// never guessed merely because every first-page call advertises it.
    cursor: Option<String>,
}

fn default_list_limit() -> usize {
    200
}

#[async_trait]
impl Tool for FsListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.list".into(),
            description: "List workspace files (hides .focus-agent and .git). Overflow returns an artifact_ref; read further lines with artifact.read.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
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
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ListArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.list args: {e}")))?;
        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.list", &args.path,
            )));
        }
        if let Some(cursor) = args.cursor.as_deref() {
            return self
                .page_from_snapshot(run_id, call_id, cursor, limit)
                .await;
        }
        let path = match self.workspace.resolve_relative(&args.path).await {
            Ok(path) => path,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.list", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let mut reader = match fs::read_dir(&path).await {
            Ok(reader) => reader,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::NotADirectory =>
            {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.list", &args.path).await,
                ));
            }
            Err(e) => {
                return Err(AgentError::Io(format!("list {}: {e}", path.display())));
            }
        };

        let mut entries = Vec::new();
        let mut scan_incomplete = false;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read directory: {e}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_hidden_name(&name) {
                continue;
            }
            if entries.len() >= MAX_LIST_ENTRIES {
                scan_incomplete = true;
                break;
            }
            let metadata = entry.metadata().await.ok();
            let kind = metadata
                .as_ref()
                .map(|m| {
                    if m.is_dir() {
                        "dir"
                    } else if m.is_file() {
                        "file"
                    } else {
                        "other"
                    }
                })
                .unwrap_or("unknown");
            entries.push(format!("{kind}\t{name}"));
        }
        entries.sort();

        let visible = entries.iter().take(limit).cloned().collect::<Vec<_>>();
        let full = entries.join("\n");
        let artifact_ref = if entries.len() > limit {
            Some(
                self.workspace
                    .write_artifact(run_id, "fs-list", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{limit}")), true),
            None => (None, false),
        };
        let coverage_note = if scan_incomplete {
            " (PARTIAL scan: directory entry budget reached; do not treat this as the complete directory)"
        } else {
            ""
        };

        // 目录身份戳：path@digest 让重复列举能被证据前沿识别为同版本
        // 冗余，而不是无身份的纯 stdout。根目录的相对路径是空串，用
        // "." 表示。
        let listed_relative = display_relative(&self.workspace, &path);
        let listed = if listed_relative.is_empty() {
            ".".to_string()
        } else {
            listed_relative
        };
        let list_revision = content_digest(full.as_bytes());

        // Body-level coverage statement (F04): summary/metadata never reach
        // the model, so a budget-truncated listing must say so in the body,
        // and an overflowing listing must name its continuation there.
        let mut clauses: Vec<String> = Vec::new();
        if scan_incomplete {
            let scope = if entries.is_empty() {
                "this is not an empty directory"
            } else {
                "this is not the complete directory"
            };
            clauses.push(format!(
                "PARTIAL listing: directory entry budget reached; {scope} ({listed})"
            ));
        }
        if let Some(reference) = &artifact_ref {
            clauses.push(format!(
                "continue with artifact.read reference={reference} start_line={}",
                visible.len() + 1
            ));
        }
        let coverage = coverage_footer(clauses);

        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed {} entries in {}{coverage_note}",
                entries.len(),
                display_relative(&self.workspace, &path)
            ),
            model_content: with_coverage_footer(
                if entries.is_empty() && scan_incomplete {
                    "no entries in the scanned prefix".to_string()
                } else {
                    visible.join("\n")
                },
                coverage,
            ),
            artifact_ref,
            metadata: json!({
                // digest 对完整 listing 计算：visible 只是分页窗口，
                // 窗口外的条目变化同样改变目录身份。
                "path": listed,
                "revision": list_revision,
                "entry_count": entries.len(),
                "returned": visible.len(),
                "has_more": has_more,
                "next_start_line": has_more.then_some(visible.len() + 1),
                "cursor": cursor,
                "scan_incomplete": scan_incomplete,
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(&listed, Some(list_revision.clone()))])
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

impl FsListTool {
    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`). Pages come from the immutable snapshot,
    /// so later changes to the directory cannot cause duplicates or gaps
    /// between pages.
    async fn page_from_snapshot(
        &self,
        run_id: RunId,
        call_id: &str,
        cursor: &str,
        limit: usize,
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
            .take(limit)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        // Paging semantics in the body (F04): every page names its range
        // and snapshot identity; a middle page names its continuation and
        // the final page carries an explicit end marker.
        let coverage = if has_more {
            coverage_footer(vec![format!(
                "entries {}-{} of {} (snapshot {reference}); continue with fs.list cursor={reference}#{next_offset}",
                offset + 1,
                next_offset,
                lines.len()
            )])
        } else {
            coverage_footer(vec![format!(
                "end of listing ({next_offset} total, snapshot {reference})"
            )])
        };

        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed entries {}-{} of {} (snapshot)",
                offset + 1,
                next_offset,
                lines.len()
            ),
            model_content: with_coverage_footer(
                if page.is_empty() {
                    "no more entries".to_string()
                } else {
                    page.join("\n")
                },
                coverage,
            ),
            artifact_ref: Some(reference.to_string()),
            metadata: json!({
                "entry_count": lines.len(),
                "returned": page.len(),
                "has_more": has_more,
                "cursor": next_cursor,
                "scan_incomplete": false,
            }),
        };
        // Snapshot pages describe no fresh directory identity; the stamp
        // keeps the explicit read-only bound so the native channel and the
        // legacy derivation agree on every `fs.list` outcome.
        output.set_native_execution_facts(
            ToolExecutionFacts::empty()
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

pub struct FsReadTool {
    workspace: Workspace,
}

impl FsReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_start_line")]
    start_line: usize,
    #[serde(default = "default_end_line")]
    end_line: usize,
}

fn default_start_line() -> usize {
    1
}
fn default_end_line() -> usize {
    200
}

/// Compact physical newline map for the same logical lines `str::lines`
/// renders. It is only shown for mixed-EOL files: `C` = CRLF, `L` = LF,
/// `N` = no terminating newline. At most the configured 400-line window is
/// returned, so exposing exact edit evidence cannot grow with file size.
fn mixed_eol_tokens(text: &str, requested_start: usize, requested_end: usize) -> String {
    let bytes = text.as_bytes();
    let mut tokens = String::new();
    let mut line = 0usize;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(relative_newline) = bytes[cursor..].iter().position(|byte| *byte == b'\n') else {
            if (requested_start..requested_end).contains(&line) {
                tokens.push('N');
            }
            break;
        };
        let newline = cursor + relative_newline;
        if (requested_start..requested_end).contains(&line) {
            tokens.push(if newline > cursor && bytes[newline - 1] == b'\r' {
                'C'
            } else {
                'L'
            });
        }
        line += 1;
        if line >= requested_end {
            break;
        }
        cursor = newline + 1;
    }
    tokens
}

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.read".into(),
            description: "Read UTF-8 workspace file lines (not .focus-agent or .git), with an exact byte revision and line-ending style for safe follow-up edits.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }
            }),
            risk: ToolRisk::ReadOnly,
            // G2: the declared budget IS the page budget — the broker
            // clamps model_content to exactly the number the render loop
            // pages under, from one definition.
            output_budget: Some(FINAL_BODY_CHARS),
            roles: vec![ToolSemanticRole::ReadResource],
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
        let args: ReadArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.read args: {e}")))?;
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        // Compare the inclusive span as a difference so an adversarial
        // `usize::MAX` end cannot overflow before the boundedness check.
        if args.end_line - args.start_line >= MAX_READ_LINES {
            return Err(AgentError::InvalidRequest(format!(
                "fs.read is limited to {MAX_READ_LINES} lines per call"
            )));
        }
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.read", &args.path,
            )));
        }

        // Validation and open are fused into a directory-handle-relative
        // descent; the size check and the content read both go through the
        // pinned handle, so a link swap cannot redirect the read.
        let confined = match self.workspace.confined_open_read(&args.path).await {
            Ok(confined) => confined,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.read", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let metadata = confined.metadata().map_err(|e| {
            AgentError::Io(format!("metadata {}: {e}", confined.display().display()))
        })?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; use a narrower/specialized tool for files above {} bytes",
                metadata.len(),
                MAX_READ_BYTES
            )));
        }

        use tokio::io::AsyncReadExt;
        let display_path = confined.display().to_path_buf();
        let file = confined.into_tokio();
        let mut text = String::new();
        file.take(MAX_READ_BYTES + 1)
            .read_to_string(&mut text)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", display_path.display())))?;
        if text.len() as u64 > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file grew beyond the fs.read limit of {MAX_READ_BYTES} bytes while it was read"
            )));
        }
        let line_ending = LineEnding::detect(&text);
        let requested_start = args.start_line.saturating_sub(1);
        let requested_end = args.end_line;
        let line_count = text.lines().count();
        let window_end_visible = requested_end.min(line_count);

        let relative = display_relative(&self.workspace, &display_path);
        let quoted_relative = model_json_string(&relative);
        let revision = content_digest(text.as_bytes());
        let mixed = line_ending == LineEnding::Mixed;
        let eol_label = " eol_tokens(C=CRLF,L=LF,N=none)=";
        let digits_of = |mut value: usize| -> usize {
            let mut width = 1;
            while value >= 10 {
                value /= 10;
                width += 1;
            }
            width
        };

        // G2: page under the FINAL model-content budget. The header and
        // footer reservations are computed with the SAME builders that
        // render the final page (worst-case numbers, real path), so the
        // reservation cannot drift from the real costs and the final body
        // reaches the model verbatim — the broker's head+tail preview
        // never has to cut an fs.read page.
        let mut header_chars = "file=".len()
            + quoted_relative.chars().count()
            + " revision=".len()
            + revision.chars().count()
            + " line_ending=".len()
            + line_ending.as_str().chars().count();
        if mixed {
            // At most one token per window line.
            header_chars += eol_label.len() + requested_end.saturating_sub(requested_start);
        }
        header_chars += " lines=".len()
            + digits_of(requested_start + 1)
            + 1
            + digits_of(window_end_visible)
            + 1
            + digits_of(line_count);
        let worst_footer = coverage_footer(vec![
            format!(
                "requested lines {}-{}; showing lines {}-{} at the page budget",
                requested_start + 1,
                window_end_visible,
                requested_start + 1,
                window_end_visible
            ),
            format!("line {line_count} exceeds the page budget and is not shown"),
            format!(
                "continue with fs.read path={relative} start_line={window_end_visible} end_line={}",
                requested_end.max(line_count)
            ),
        ])
        .map(|footer| footer.chars().count() + 1)
        .unwrap_or(0);
        let content_budget = FINAL_BODY_CHARS.saturating_sub(header_chars + worst_footer);

        // G3: capture stops at the FIRST unshowable position. Whole lines
        // are the unit here, so a line either fits the remaining page
        // budget or closes the page; a line longer than the WHOLE budget
        // can never be shown by fs.read at all and is skipped only with an
        // explicit declaration in the body. Source line identity is the
        // true file line number throughout — no renumbering.
        let mut spans: Vec<DeliveredSpan> = Vec::new();
        let mut used_chars = 0usize;
        let mut stopped_at: Option<usize> = None;
        let mut stop_is_oversized = false;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if index < requested_start {
                continue;
            }
            if number > requested_end {
                break;
            }
            let chars = line.chars().count();
            if chars > content_budget {
                stopped_at = Some(number);
                stop_is_oversized = true;
                break;
            }
            let envelope = rendered_prefix_chars(number) + 1;
            if used_chars + envelope + chars > content_budget {
                stopped_at = Some(number);
                break;
            }
            used_chars += envelope + chars;
            spans.push(DeliveredSpan {
                line: number,
                shown_to: line.len(),
                complete: true,
                text: line.to_string(),
            });
        }
        // The delivered-position authority: the continuation names the
        // first undelivered line fs.read CAN still deliver, and it is a
        // FILE-walk cursor — a fully delivered window of a longer file
        // still continues past the window (the walk must be able to reach
        // the real end; "no more" only at true EOF). A budget stop resumes
        // on the stopped line; an oversized line is declared undeliverable
        // and the cursor moves past it — never silently.
        let delivered_last_line = spans.last().map(|span| span.line);
        let has_more =
            stopped_at.is_some() || delivered_last_line.is_some_and(|last| last < line_count);
        let continuation = {
            let next = match stopped_at {
                Some(stop) if stop_is_oversized => Some(stop + 1),
                Some(stop) => Some(stop),
                None => delivered_last_line.map(|last| last + 1),
            };
            next.filter(|&next| next <= line_count).map(|next| {
                // Inside the requested window, finish that window
                // first; past it, continue with the tool's own
                // default page size (200 lines), bounded by the file.
                let end = if next <= window_end_visible {
                    requested_end
                } else {
                    (next + FS_READ_PAGE_LINES - 1).min(line_count)
                };
                (next, end)
            })
        };

        // The finalized page: the shared loop measures the FINAL envelope
        // (header claim + numbered spans + footer) and drops trailing
        // spans on an overrun; every claim below is derived from the kept
        // spans only.
        let page = finalize_within_budget(
            spans,
            |spans: &[DeliveredSpan]| -> (String, Option<String>) {
                let first = spans.first().map(|span| span.line);
                let last = spans.last().map(|span| span.line);
                let mut content = format!(
                    "file={quoted_relative} revision={revision} line_ending={}",
                    line_ending.as_str()
                );
                if mixed && let (Some(first), Some(last)) = (first, last) {
                    let tokens = mixed_eol_tokens(&text, first - 1, last);
                    content.push_str(eol_label);
                    content.push_str(&tokens);
                }
                match (first, last) {
                    (Some(first), Some(last)) => {
                        content.push_str(&format!(" lines={first}-{last}/{line_count}"));
                    }
                    _ if line_count == 0 => {
                        content.push_str(" lines=0-0/0");
                    }
                    _ => {}
                }
                if !spans.is_empty() {
                    content.push('\n');
                    content.push_str(&render_numbered_spans(spans));
                }
                let mut clauses: Vec<String> = Vec::new();
                if stop_is_oversized {
                    if let Some(stop) = stopped_at {
                        clauses.push(format!(
                            "line {stop} exceeds the page budget and is not shown"
                        ));
                    }
                } else if let (Some(_stop), Some(first), Some(last)) = (stopped_at, first, last) {
                    clauses.push(format!(
                        "requested lines {}-{}; showing lines {first}-{last} at the page budget",
                        requested_start + 1,
                        window_end_visible
                    ));
                }
                if let Some((next, end)) = continuation {
                    clauses.push(format!(
                        "continue with fs.read path={relative} start_line={next} end_line={end}"
                    ));
                }
                (content, coverage_footer(clauses))
            },
        );

        let start = requested_start.min(line_count);
        let end = window_end_visible;
        let returned_start = page.spans.first().map(|span| span.line as u64);
        let returned_end = page.spans.last().map(|span| span.line as u64);
        let covers_file = match line_count {
            0 => true,
            // Whole-file coverage only when the file was delivered
            // contiguously from its first line to its last.
            _ => returned_start == Some(1) && returned_end == Some(line_count as u64),
        };

        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: format!(
                "read lines {}-{} of {}",
                returned_start.unwrap_or(start as u64 + 1),
                returned_end.unwrap_or(end as u64),
                relative
            ),
            model_content: page.body,
            artifact_ref: None,
            metadata: json!({
                "path": relative,
                "line_count": line_count,
                "bytes": text.len(),
                "line_ending": line_ending.as_str(),
                // The content revision (SHA-256 hex): stable for the same
                // bytes, changes with any edit — the patch tool's
                // `base_revision` precondition is checked against this.
                "revision": revision,
                // G2: the window metadata names the DELIVERED range —
                // the model-visible proof — never the requested range.
                "start_line": returned_start,
                "end_line": returned_end,
                "covers_file": covers_file,
                "has_more": has_more,
                "next_start_line": continuation.map(|(next, _)| next as u64),
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(&relative, Some(revision.clone()))])
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

pub struct FsWriteTool {
    workspace: Workspace,
}

impl FsWriteTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }

    async fn execute_inner(
        &self,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
    ) -> AgentResult<ToolOutcome> {
        let args: WriteArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.write args: {e}")))?;
        if args.content.len() > MAX_WRITE_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "fs.write content is {} bytes; the limit is {MAX_WRITE_BYTES} bytes",
                args.content.len()
            )));
        }
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.write", &args.path,
            )));
        }
        let path = self.workspace.resolve_mutation(&args.path).await?;
        let transaction = match self
            .workspace
            .begin_mutation("fs.write", "write", &args.path)
            .await
        {
            Ok(transaction) => transaction,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_parent_output(&self.workspace, call_id, "fs.write", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        // Computation is staged, the side effect is not applied yet: the
        // runtime owns the commit after the generation fence. Production
        // dispatches attach Core's stable identity; direct legacy tests can
        // still exercise the transaction primitive without one.
        let prepared = match effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(args.content.as_bytes(), context)
                    .await?
            }
            None => transaction.prepare(args.content.as_bytes()).await?,
        };
        let effect: Box<dyn Effect> = Box::new(prepared);
        let relative = display_relative(&self.workspace, &path);
        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: format!("wrote {} bytes to {}", args.content.len(), relative),
            model_content: format!("file updated: {relative}"),
            artifact_ref: None,
            metadata: json!({
                "path": relative,
                "bytes": args.content.len(),
                "revision": content_digest(args.content.as_bytes()),
                "line_ending": LineEnding::detect(&args.content).as_str(),
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(
                relative.as_str(),
                Some(content_digest(args.content.as_bytes())),
            )])
            .with_verification(false)
            .with_mutation_bound(true),
        );
        Ok(ToolOutcome::PreparedEffect { output, effect })
    }
}

/// Transactional creation of one directory component. It deliberately does
/// not implement `mkdir -p`: every visible topology change has one Core
/// intent, one pinned parent and one recovery identity.
pub struct FsMkdirTool {
    workspace: Workspace,
}

impl FsMkdirTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
}

#[async_trait]
impl Tool for FsMkdirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.mkdir".into(),
            description: "Create exactly one workspace directory. Its immediate parent must already exist; an existing directory succeeds without mutation.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path"}
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
        let args: MkdirArgs = serde_json::from_value(arguments)
            .map_err(|error| AgentError::InvalidRequest(format!("fs.mkdir args: {error}")))?;
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.mkdir", &args.path,
            )));
        }
        let preparation = match self
            .workspace
            .prepare_directory_creation("fs.mkdir", &args.path, effect_context)
            .await
        {
            Ok(preparation) => preparation,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_parent_output(&self.workspace, call_id, "fs.mkdir", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let relative = preparation.relative_path().to_string();
        match preparation {
            DirectoryCreationPreparation::AlreadyExists { .. } => {
                let mut output = ToolOutput {
                    call_id: call_id.into(),
                    tool_name: "fs.mkdir".into(),
                    ok: true,
                    summary: format!("directory already exists: {relative}"),
                    model_content: format!("directory already exists: {relative}"),
                    artifact_ref: None,
                    metadata: json!({
                        "path": relative,
                        "created": false,
                        "entry_kind": "directory",
                        "mutates_workspace": false,
                        "verification": false,
                    }),
                };
                output.set_native_execution_facts(
                    ToolExecutionFacts::from_resource_touches([(
                        relative.as_str(),
                        Option::<String>::None,
                    )])
                    .with_verification(false)
                    .with_mutation_bound(false),
                );
                Ok(ToolOutcome::Value(output))
            }
            DirectoryCreationPreparation::Prepared(prepared) => {
                let mut output = ToolOutput {
                    call_id: call_id.into(),
                    tool_name: "fs.mkdir".into(),
                    ok: true,
                    summary: format!("created directory {relative}"),
                    model_content: format!("directory created: {relative}"),
                    artifact_ref: None,
                    metadata: json!({
                        "path": relative,
                        "created": true,
                        "entry_kind": "directory",
                        "mutates_workspace": true,
                        "verification": false,
                    }),
                };
                output.set_native_execution_facts(
                    ToolExecutionFacts::from_resource_touches([(
                        relative.as_str(),
                        Option::<String>::None,
                    )])
                    .with_verification(false)
                    .with_mutation_bound(true),
                );
                Ok(ToolOutcome::PreparedEffect {
                    output,
                    effect: prepared,
                })
            }
        }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for FsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "Write/replace a UTF-8 text file inside an existing workspace directory (maximum 4 MiB; parent directories are never created implicitly). Prefer edit.patch for existing files.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
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
        self.execute_inner(call_id, arguments, effect_context).await
    }
}

fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest, ToolFailureClass};
    use serde_json::json;

    #[test]
    fn mixed_eol_map_matches_rendered_line_windows_at_boundaries() {
        assert_eq!(mixed_eol_tokens("", 0, 400), "");
        assert_eq!(mixed_eol_tokens("a\r\nb\nc", 0, 400), "CLN");
        assert_eq!(mixed_eol_tokens("a\r\nb\nc", 1, 2), "L");
        assert_eq!(mixed_eol_tokens("a\r\nb\n", 0, 400), "CL");
        assert_eq!(mixed_eol_tokens("\n\r\nx", 0, 400), "LCN");
        assert_eq!(mixed_eol_tokens("a\rb", 0, 400), "N");
    }

    /// Every fs outcome that stamps native facts must agree with what the
    /// legacy key derivation would produce — the two channels are locked
    /// together until the legacy stamps are retired.
    #[tokio::test]
    async fn native_facts_match_the_legacy_derivation_on_every_fs_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();

        let write = FsWriteTool::new(workspace.clone());
        let outcome = write
            .execute(
                run_id,
                "w",
                json!({"path": "notes.txt", "content": "first"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));

        let read = FsReadTool::new(workspace.clone());
        let outcome = read
            .execute(
                run_id,
                "r",
                json!({"path": "notes.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read must return a value outcome");
        };
        crate::tools::assert_native_facts_match_derivation(&output);

        let list = FsListTool::new(workspace.clone());
        let outcome = list
            .execute(
                run_id,
                "l",
                json!({"path": "."}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list must return a value outcome");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
    }

    #[tokio::test]
    async fn fs_write_journals_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace.clone());
        let run_id = RunId::new();

        let write = |path: &str, content: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": path, "content": content}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Writing a new file stages the mutation; the runtime would commit
        // it after validating the operation — the test plays that role.
        let outcome = write("notes.txt", "first").await.unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        assert!(output.ok);
        assert!(output.heats_working_set());
        assert_eq!(output.metadata["path"], "notes.txt");
        assert_eq!(
            output.metadata["revision"].as_str().unwrap().len(),
            64,
            "write stamps a content revision"
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
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt"))
                .await
                .unwrap(),
            "first"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["tool"], "fs.write");
        assert_eq!(record["action"], "write");
        assert_eq!(record["path"], "notes.txt");
        assert_eq!(record["bytes_before"], 0);
        assert_eq!(record["bytes_after"], 5);

        // Overwriting captures the previous content as the journal backup.
        let outcome = write("notes.txt", "second").await.unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("fs.write must prepare a committed effect");
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
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(record["old_content"], "first");
    }

    #[tokio::test]
    async fn fs_write_rejects_state_dir_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace);
        let run_id = RunId::new();
        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": ".focus-agent/traces.jsonl", "content": "x"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        };
        let result = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::Value(output) = result else {
            panic!("hidden writes must refuse without staging");
        };
        assert!(!output.ok);
        assert_eq!(output.failure_class(), Some(ToolFailureClass::HiddenPath));
    }

    #[tokio::test]
    async fn fs_read_reports_a_stable_content_revision() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello revision\n").unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let run_id = RunId::new();

        let read = |path: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.read".into(),
                arguments: json!({"path": path}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        let first = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.read returns a plain value");
        };
        let revision_a = output.metadata["revision"].as_str().unwrap().to_string();
        assert_eq!(revision_a.len(), 64, "a full SHA-256 hex revision");
        assert_eq!(
            output.metadata["path"].as_str(),
            Some("notes.txt"),
            "fs.read must stamp the workspace-relative path; ingest cannot recover it from numbered lines"
        );

        // Same bytes, same revision.
        let second = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.read returns a plain value");
        };
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "reading the same content must not change the revision"
        );

        // Any edit changes the revision.
        std::fs::write(dir.path().join("notes.txt"), "hello revision!\n").unwrap();
        let third = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.read returns a plain value");
        };
        assert_ne!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "an edited file must report a different revision"
        );

        // The revision is exactly the digest helper the patch tool will
        // use for its `base_revision` precondition.
        let bytes = std::fs::read(dir.path().join("notes.txt")).unwrap();
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            content_digest(&bytes)
        );
    }

    #[tokio::test]
    async fn fs_read_reports_crlf_without_exposing_carriage_returns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("windows.txt"), b"one\r\ntwo\r\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "windows.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };
        assert_eq!(output.metadata["line_ending"], "crlf");
        assert!(!output.model_content.contains('\r'));
        assert!(
            output
                .model_content
                .starts_with("file=\"windows.txt\" revision=")
        );
        assert!(output.model_content.contains(" line_ending=crlf"));
        assert_eq!(
            output.metadata["revision"],
            content_digest(b"one\r\ntwo\r\n")
        );
    }

    #[tokio::test]
    async fn fs_read_exposes_a_bounded_mixed_eol_map_without_shell() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("mixed.txt"), b"one\r\ntwo\nthree\r\nfour")
            .await
            .unwrap();
        let tool = FsReadTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "mixed.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };
        assert_eq!(output.metadata["line_ending"], "mixed");
        assert!(
            output
                .model_content
                .contains("line_ending=mixed eol_tokens(C=CRLF,L=LF,N=none)=CLCN")
        );
        assert!(output.model_content.contains(&format!(
            "revision={}",
            content_digest(b"one\r\ntwo\nthree\r\nfour")
        )));
        assert!(!output.model_content.contains('\r'));
    }

    #[tokio::test]
    async fn fs_read_newline_dense_file_returns_only_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let line_count = MAX_READ_BYTES as usize;
        std::fs::write(dir.path().join("dense.txt"), vec![b'\n'; line_count]).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let start_line = line_count - MAX_READ_LINES + 1;
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "dense.txt",
                    "start_line": start_line,
                    "end_line": line_count,
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };

        assert_eq!(output.metadata["line_count"], line_count);
        assert_eq!(output.model_content.lines().count(), MAX_READ_LINES + 1);
        assert!(
            output
                .model_content
                .lines()
                .nth(1)
                .is_some_and(|line| line.starts_with(&format!("{start_line:>6} | "))),
            "the selected window must retain its original line numbers"
        );
        assert!(
            output
                .model_content
                .ends_with(&format!("{line_count:>6} | ")),
            "the selected window must end at the requested line"
        );
        assert!(
            output.model_content.len() < 8 * 1024,
            "a newline-dense file must produce output proportional to the requested window"
        );
    }

    #[tokio::test]
    async fn fs_read_stamps_the_returned_window_and_keeps_the_whole_file_revision() {
        let dir = tempfile::tempdir().unwrap();
        let body = (1..=30)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("window.rs"), &body).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "window.rs",
                    "start_line": 10,
                    "end_line": 12,
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };

        assert_eq!(output.metadata["start_line"], 10);
        assert_eq!(output.metadata["end_line"], 12);
        assert_eq!(output.metadata["line_count"], 30);
        assert_eq!(output.metadata["covers_file"], false);
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            content_digest(body.as_bytes()),
            "edit CAS still uses the whole-file digest"
        );
        assert!(output.model_content.contains("lines=10-12/30"));
        assert!(output.model_content.contains("    10 | line-10"));
        assert!(output.model_content.contains("    12 | line-12"));
        assert!(!output.model_content.contains("     9 | line-9"));
        assert!(!output.model_content.contains("    13 | line-13"));
        assert_eq!(output.file_line_range(), Some((10, 12)));
    }

    #[tokio::test]
    async fn fs_read_rejects_files_above_the_workspace_mutation_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("too-large.txt"),
            vec![b'x'; MAX_MUTATION_BYTES + 1],
        )
        .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);

        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "too-large.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentError::InvalidRequest(message)
                if message.contains(&MAX_READ_BYTES.to_string())),
            "fs.read and workspace mutations must reject at the same byte boundary"
        );
    }

    #[tokio::test]
    async fn fs_read_rejects_an_overflowing_line_window() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.txt"), b"one\n")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);

        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "small.txt",
                    "start_line": 1,
                    "end_line": usize::MAX,
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::InvalidRequest(message) if message.contains("limited"))
        );
    }

    #[tokio::test]
    async fn fs_write_rejects_content_over_the_real_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace);
        let result = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "too-large.txt", "content": "x".repeat(MAX_WRITE_BYTES + 1)}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        assert!(!dir.path().join("too-large.txt").exists());
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn fs_list_pages_a_consistent_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        // List a subdirectory so the runtime state dir (.focus-agent) does
        // not enter the count.
        std::fs::create_dir(dir.path().join("d")).unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join("d").join(format!("file-{i:02}.txt")), "x").unwrap();
        }
        let tool = FsListTool::new(workspace.clone());
        let run_id = RunId::new();

        let list = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.list".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Page 1: limit 4 of 10 entries → spills a snapshot + cursor.
        let first = list(json!({"path": "d", "limit": 4})).await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["entry_count"], 10);
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        assert_eq!(output.metadata["scan_incomplete"], false);
        assert!(
            output.artifact_ref.is_some(),
            "an overflowing listing must spill a snapshot"
        );
        let cursor = output.metadata["cursor"].as_str().unwrap().to_string();
        let first_content = output.model_content.clone();

        // The directory changes between pages — paging must not notice.
        std::fs::write(dir.path().join("d").join("zz-new.txt"), "x").unwrap();
        std::fs::remove_file(dir.path().join("d").join("file-00.txt")).unwrap();

        // Page 2 from the snapshot: exactly the next 4 snapshot entries,
        // no duplicates from page 1, no gaps, no new file leaking in.
        let second = list(json!({"path": "d", "limit": 4, "cursor": cursor}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.list returns a plain value");
        };
        let second_lines: Vec<&str> = output.model_content.lines().collect();
        assert_eq!(
            second_lines.len(),
            5,
            "4 entries plus the coverage footer line"
        );
        assert!(
            second_lines
                .last()
                .is_some_and(|line| line.contains("entries 5-8 of 10 (snapshot")
                    && line.contains("continue with fs.list cursor=")),
            "a middle page must carry range, snapshot identity and continuation: {:?}",
            second_lines.last()
        );
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        let first_lines: Vec<&str> = first_content.lines().collect();
        assert!(
            second_lines.iter().all(|line| !first_lines.contains(line)),
            "pages must not overlap: {first_lines:?} vs {second_lines:?}"
        );
        assert!(
            !second_lines.iter().any(|line| line.contains("zz-new")),
            "the snapshot must not see later directory changes"
        );
        let cursor2 = output.metadata["cursor"].as_str().unwrap().to_string();

        // Page 3 drains the remaining 2 entries.
        let third = list(json!({"path": "d", "limit": 4, "cursor": cursor2}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["returned"], 2);
        assert_eq!(output.metadata["has_more"], false);
        assert!(output.metadata["cursor"].is_null());
        assert!(
            output
                .model_content
                .contains("end of listing (10 total, snapshot"),
            "the final page must carry the end marker in the body: {}",
            output.model_content
        );

        // A corrupted cursor (offset beyond the snapshot) is a clean error.
        let bad = format!("{}#9999", output.artifact_ref.as_deref().unwrap());
        let result = list(json!({"path": "d", "limit": 4, "cursor": bad})).await;
        assert!(result.is_err(), "a cursor past the snapshot must error");
    }

    #[tokio::test]
    async fn fs_list_marks_partial_when_the_entry_budget_stops_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("many")).unwrap();
        for i in 0..=MAX_LIST_ENTRIES {
            std::fs::write(dir.path().join("many").join(format!("f-{i:04}.txt")), "x").unwrap();
        }
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsListTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "many", "limit": 8}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list returns a value");
        };
        assert_eq!(output.metadata["scan_incomplete"], true);
        assert_eq!(output.metadata["has_more"], true);
        assert_eq!(output.metadata["entry_count"], MAX_LIST_ENTRIES);
        assert!(
            output.summary.contains("PARTIAL scan"),
            "a capped directory walk must not look complete: {}",
            output.summary
        );
        assert!(
            output
                .summary
                .contains("do not treat this as the complete directory"),
            "{}",
            output.summary
        );
    }

    /// F04 反例（fs.list 版）：非空但被条目预算截断的列表在正文里也
    /// 必须声明不完整——`model_content` 是唯一进 TurnFrame 的字段，
    /// summary/metadata 到不了模型。
    #[tokio::test]
    async fn a_partial_nonempty_listing_declares_incompleteness_in_the_model_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("many")).unwrap();
        for i in 0..=MAX_LIST_ENTRIES {
            std::fs::write(dir.path().join("many").join(format!("f-{i:04}.txt")), "x").unwrap();
        }
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsListTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "many", "limit": 8}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list returns a value");
        };
        assert!(
            output.model_content.contains("f-0000"),
            "entries stay visible: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("[coverage]") && output.model_content.contains("PARTIAL"),
            "a non-empty partial listing must carry the coverage statement in the body: {}",
            output.model_content
        );
        assert!(
            output
                .model_content
                .contains("not the complete directory (many)"),
            "the body must name the scope of the partial listing: {}",
            output.model_content
        );
    }

    #[tokio::test]
    async fn root_list_hides_runtime_and_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsListTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": ""}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list returns a value");
        };
        assert!(output.ok);
        assert!(output.model_content.contains("README.md"));
        assert!(!output.model_content.contains(".focus-agent"));
        assert!(!output.model_content.contains(".git"));
    }

    #[tokio::test]
    async fn fs_mkdir_prepares_one_zero_byte_effect_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsMkdirTool::new(workspace.clone());
        let outcome = tool
            .execute(
                RunId::new(),
                "mkdir-1",
                json!({"path": "src"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("an absent directory must produce a prepared effect");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert!(!workspace.root().join("src").exists());
        assert_eq!(
            effect.actual_workspace_writes().unwrap(),
            vec![agent_contracts::ActualWorkspaceWrite {
                path: "src".into(),
                bytes: 0,
            }]
        );
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                ..
            }
        ));
        assert!(workspace.root().join("src").is_dir());

        let outcome = tool
            .execute(
                RunId::new(),
                "mkdir-2",
                json!({"path": "src"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("an existing directory must be an idempotent value");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert_eq!(output.metadata["created"], false);
        assert!(!output.may_mutate_workspace());
    }

    #[tokio::test]
    async fn fs_mkdir_and_write_explain_missing_parent_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mkdir = FsMkdirTool::new(workspace.clone());
        let ToolOutcome::Value(missing) = mkdir
            .execute(
                RunId::new(),
                "mkdir",
                json!({"path": "missing/child"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("a missing parent must be a typed refusal");
        };
        assert_eq!(
            missing.failure_class(),
            Some(ToolFailureClass::PathNotFound)
        );
        assert_eq!(missing.metadata["next_directory"], "missing");
        assert!(missing.model_content.contains("fs.mkdir"));
        assert!(!workspace.root().join("missing").exists());

        let write = FsWriteTool::new(workspace);
        let ToolOutcome::Value(missing) = write
            .execute(
                RunId::new(),
                "write",
                json!({"path": "missing/file.txt", "content": "x"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("a missing write parent must be a typed refusal");
        };
        assert_eq!(
            missing.failure_class(),
            Some(ToolFailureClass::PathNotFound)
        );
        assert_eq!(missing.metadata["next_directory"], "missing");
        assert!(missing.model_content.contains("fs.mkdir"));
    }

    #[tokio::test]
    async fn read_hidden_and_missing_paths_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "ok\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let run_id = RunId::new();
        let hidden = tool
            .execute(
                run_id,
                "c",
                json!({"path": ".focus-agent/changes.jsonl"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = hidden else {
            panic!("hidden read is a value");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::HiddenPath));

        let missing = tool
            .execute(
                run_id,
                "c",
                json!({"path": "src/lib.rs"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = missing else {
            panic!("missing read is a value");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::PathNotFound));
        assert!(
            output.model_content.contains("src.txt") || output.model_content.contains("parent")
        );
        assert!(
            !output.model_content.contains("Cargo.toml")
                || output.model_content.contains("Do not invent")
        );
    }

    // -- G2/G3 fs.read regressions (tenth batch follow-up) -------------------
    //
    // fs.read must page under the FINAL model-content budget (the same cap
    // the trusted broker and the runtime last-line guard enforce), so its
    // `lines=S-E/N` claim and its continuation describe only source content
    // actually present in the final delivered text. The probes below walk
    // the REAL tool→broker path and follow ONLY the continuations the
    // model-visible body offers.

    /// Run one fs.read call through the REAL trusted output broker, exactly
    /// as the kernel does (the executed tool's declared output budget).
    async fn read_fs_through_broker(
        tool: &FsReadTool,
        broker: &agent_workspace::WorkspaceOutputBroker,
        run_id: RunId,
        args: Value,
    ) -> ToolOutput {
        let outcome = tool
            .execute(run_id, "c", args, None, CancellationToken::new())
            .await
            .expect("fs.read must execute");
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a plain value");
        };
        use agent_contracts::OutputBroker as _;
        broker
            .bound(
                run_id,
                Some(agent_contracts::MAX_TOOL_MODEL_CONTENT_CHARS),
                output,
            )
            .await
    }

    /// Parse the header claim `lines=S-E/TOTAL` of an fs.read body.
    fn fs_claimed_lines(body: &str) -> Option<(usize, usize, usize)> {
        let at = body.find("lines=")? + "lines=".len();
        let segment: String = body[at..]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '/')
            .collect();
        let mut parts = segment.split('/');
        let mut range = parts.next()?.split('-');
        let start = range.next()?.parse().ok()?;
        let end = range.next()?.parse().ok()?;
        let total = parts.next()?.parse().ok()?;
        Some((start, end, total))
    }

    /// Parse a delivered page body into its rendered (source line, text)
    /// entries: the `{:>6} | ` numbered lines, excluding the file header
    /// and the coverage footer.
    fn fs_rendered_entries(body: &str) -> Vec<(usize, String)> {
        body.lines()
            .filter_map(|line| {
                if line.starts_with("file=") || line.starts_with('[') {
                    return None;
                }
                let (number, text) = line.split_once(" | ")?;
                Some((number.trim().parse::<usize>().ok()?, text.to_string()))
            })
            .collect()
    }

    /// Extract the continuation arguments EXACTLY as the model-visible body
    /// states them: `continue with fs.read path=... start_line=... end_line=...`.
    fn fs_continuation_args(content: &str) -> Value {
        let marker = "continue with fs.read ";
        let at = content
            .find(marker)
            .unwrap_or_else(|| panic!("the page must offer a continuation in its body: {content}"));
        let clause = content[at + marker.len()..]
            .split(['\n', ';'])
            .next()
            .unwrap_or_default();
        let mut path = None;
        let mut start_line = None;
        let mut end_line = None;
        for token in clause.split_whitespace() {
            if let Some(value) = token.strip_prefix("path=") {
                path = Some(value.to_string());
            } else if let Some(value) = token.strip_prefix("start_line=") {
                start_line = value.parse::<usize>().ok();
            } else if let Some(value) = token.strip_prefix("end_line=") {
                end_line = value.parse::<usize>().ok();
            }
        }
        serde_json::json!({
            "path": path.expect("the continuation must name the path"),
            "start_line": start_line.expect("the continuation must name start_line"),
            "end_line": end_line.expect("the continuation must name end_line"),
        })
    }

    fn fs_page_body_checks(output: &ToolOutput) {
        assert!(
            output.model_content.chars().count() <= agent_contracts::MAX_TOOL_MODEL_CONTENT_CHARS,
            "the FINAL model-visible body must stay within the model-content budget: {} chars",
            output.model_content.chars().count()
        );
        assert!(
            !output.model_content.contains("output broker truncated")
                && !output.model_content.contains("runtime truncated"),
            "a page under the final budget must reach the model verbatim — no head+tail clip: {}",
            output.summary
        );
    }

    /// G2 fs.read probe (KV-sequence shape): 600 lines of exactly 150
    /// content chars; unique block IDs at lines 100/300/500 (page middles
    /// of 200-line requests). Following ONLY the returned continuations
    /// through the real broker must deliver every line verbatim exactly
    /// once — no gaps, no overlaps — with every page inside the final
    /// budget and untrimmed.
    #[tokio::test]
    async fn fs_read_walk_delivers_every_line_via_returned_continuations() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let total = 600usize;
        let line_ids: Vec<String> = (1..=total)
            .map(|line| format!("FS2-L{line:04}-{line:x}7f{line:03}"))
            .collect();
        let mut body = String::new();
        for line in 1..=total {
            let mut text = line_ids[line - 1].clone();
            while text.chars().count() < 150 {
                text.push('x');
            }
            body.push_str(&text);
            body.push('\n');
        }
        std::fs::write(dir.path().join("big.log"), &body).unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({"path": "big.log"});
        let mut delivered: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        let mut pages = 0usize;
        loop {
            let output = read_fs_through_broker(&tool, &broker, run_id, args.clone()).await;
            fs_page_body_checks(&output);
            pages += 1;
            assert!(pages < 20, "the walk must converge: {pages} pages");
            for (number, text) in fs_rendered_entries(&output.model_content) {
                let slot = delivered.entry(number).or_default();
                assert!(
                    slot.is_empty(),
                    "line {number} delivered twice — no overlaps allowed"
                );
                *slot = text;
            }
            let has_more = output.metadata["has_more"].as_bool().unwrap();
            if !has_more {
                break;
            }
            args = fs_continuation_args(&output.model_content);
        }
        assert_eq!(
            delivered.keys().copied().collect::<Vec<_>>(),
            (1..=total).collect::<Vec<_>>(),
            "the delivered source intervals must cover lines 1..={total} exactly"
        );
        for (number, text) in &delivered {
            let mut expected = line_ids[number - 1].clone();
            while expected.chars().count() < 150 {
                expected.push('x');
            }
            assert_eq!(text, &expected, "line {number} must be delivered verbatim");
        }
        // Mid-page markers (the review's lines 100/300 positions and the
        // KV walk's 500): delivered exactly once each.
        for marker_line in [100usize, 300, 500] {
            assert!(delivered[&marker_line].contains(&line_ids[marker_line - 1]));
        }
    }

    /// G2 fs.read claim honesty: the `lines=S-E/N` header claim and the
    /// metadata window describe EXACTLY the rendered source lines of the
    /// final delivered body — never the requested-but-undelivered range —
    /// and the continuation points at the first undelivered line.
    #[tokio::test]
    async fn fs_read_claim_and_metadata_describe_the_delivered_range() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = String::new();
        for line in 1..=600usize {
            let marker = if line == 100 { "FS-CLAIM-MID-L100" } else { "" };
            let mut text = format!("row-{line:04}{marker}");
            while text.chars().count() < 150 {
                text.push('.');
            }
            body.push_str(&text);
            body.push('\n');
        }
        std::fs::write(dir.path().join("claims.log"), &body).unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let output = read_fs_through_broker(
            &tool,
            &broker,
            run_id,
            json!({"path": "claims.log", "start_line": 1, "end_line": 200}),
        )
        .await;
        fs_page_body_checks(&output);

        let entries = fs_rendered_entries(&output.model_content);
        assert!(!entries.is_empty(), "the page must deliver lines");
        let first = entries[0].0;
        let last = entries[entries.len() - 1].0;
        let claimed = fs_claimed_lines(&output.model_content)
            .expect("the header must carry the lines= claim");
        assert_eq!(
            claimed,
            (first, last, 600),
            "the claim must describe exactly the delivered range: {}",
            output.model_content
        );
        assert_eq!(output.metadata["start_line"], first as u64);
        assert_eq!(output.metadata["end_line"], last as u64);
        assert_eq!(output.metadata["line_count"], 600);
        assert_eq!(output.metadata["covers_file"], false);
        assert_eq!(output.metadata["has_more"], true);
        assert_eq!(output.metadata["next_start_line"], last as u64 + 1);
        // The next window's first line exists and was not silently shown.
        assert!(last < 200);
        // A mid-window marker inside the delivered range rides in the body.
        if last >= 100 {
            assert!(output.model_content.contains("FS-CLAIM-MID-L100"));
        }
        assert!(
            output
                .model_content
                .contains(&format!("start_line={}", last + 1)),
            "the body must carry the continuation: {}",
            output.model_content
        );
    }

    /// G3-adjacent fs.read probe: multibyte lines are budgeted by rendered
    /// CHARS (the broker trims by chars), whole lines are never cut, code
    /// points never split, and source line identity survives the walk.
    #[tokio::test]
    async fn fs_read_multibyte_pages_by_chars_without_cutting_code_points() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let total = 300usize;
        let mut body = String::new();
        for line in 1..=total {
            let marker = if line == 150 { "FS3-界-MARKER" } else { "" };
            let mut text = format!("row-{line:04}{marker}");
            while text.chars().count() < 400 {
                text.push('界');
            }
            body.push_str(&text);
            body.push('\n');
        }
        std::fs::write(dir.path().join("multibyte.log"), &body).unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({"path": "multibyte.log"});
        let mut delivered: Vec<(usize, String)> = Vec::new();
        let mut pages = 0usize;
        loop {
            let output = read_fs_through_broker(&tool, &broker, run_id, args.clone()).await;
            fs_page_body_checks(&output);
            assert!(
                !output.model_content.contains('\u{FFFD}'),
                "no page may cut a code point in half: {}",
                output.summary
            );
            pages += 1;
            assert!(pages < 40, "the walk must converge: {pages} pages");
            delivered.extend(fs_rendered_entries(&output.model_content));
            if !output.metadata["has_more"].as_bool().unwrap() {
                break;
            }
            args = fs_continuation_args(&output.model_content);
        }
        let numbers: Vec<usize> = delivered.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            numbers,
            (1..=total).collect::<Vec<_>>(),
            "source line identity is preserved, no renumbering, no gaps"
        );
        for (number, text) in &delivered {
            let mut expected = format!("row-{number:04}");
            if *number == 150 {
                expected.push_str("FS3-界-MARKER");
            }
            while expected.chars().count() < 400 {
                expected.push('界');
            }
            assert_eq!(text, &expected, "line {number} verbatim");
        }
    }

    /// A single line longer than the whole page budget cannot be shown by
    /// fs.read at all. It must be DECLARED (never silently skipped, never
    /// partially presented as if complete), the page must stay bounded,
    /// and the following short line must remain reachable under its true
    /// source number.
    #[tokio::test]
    async fn fs_read_oversized_line_is_declared_not_silently_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = "a".repeat(40_000);
        body.push('\n');
        body.push_str("FS4-short-second-line\n");
        std::fs::write(dir.path().join("oversized.log"), &body).unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let first = read_fs_through_broker(
            &tool,
            &broker,
            run_id,
            json!({"path": "oversized.log", "start_line": 1, "end_line": 2}),
        )
        .await;
        fs_page_body_checks(&first);
        assert_eq!(
            first.metadata["has_more"], true,
            "line 1 is unshown: the page must not claim completion"
        );
        assert!(
            !first.model_content.contains("FS4-short-second-line"),
            "no line after the first unshowable position may be captured: {}",
            first.model_content
        );
        assert!(
            first.model_content.contains("exceeds the page budget"),
            "the oversized line must be declared in the body: {}",
            first.model_content
        );

        // Follow the returned continuation verbatim: the short line under
        // its TRUE source number, and an honest end of the walk.
        let second = read_fs_through_broker(
            &tool,
            &broker,
            run_id,
            fs_continuation_args(&first.model_content),
        )
        .await;
        fs_page_body_checks(&second);
        let entries = fs_rendered_entries(&second.model_content);
        assert_eq!(
            entries,
            vec![(2usize, "FS4-short-second-line".to_string())],
            "source line identity preserved: {}",
            second.model_content
        );
        assert!(
            !second.model_content.contains("output broker truncated"),
            "the follow-up page reaches the model verbatim: {}",
            second.model_content
        );
    }
}

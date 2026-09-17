//! `artifact.read` — bounded line-range fetch from a run artifact.
//!
//! Tools that spill large output (fs.list, search.grep, capability.search,
//! git, shell/process logs) hand the model an opaque `artifact://`
//! reference instead of the content (invariant 4: raw tool output is not
//! prompt history). This tool is the read side of that contract: it
//! resolves the reference, confined to the run artifact store, and returns
//! a bounded, numbered line range with paging metadata — so the model can
//! walk a spilled snapshot one page at a time without ever guessing
//! filesystem paths.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::{Tool, coverage_footer, with_coverage_footer};

/// Captured window cap: at most this many artifact bytes are returned per
/// call regardless of the line range.
const MAX_READ_BYTES: usize = 2 * 1024 * 1024;
const MAX_READ_LINES: usize = 400;
/// `{:>6} | ` numbering prefix added to every rendered line; the capture
/// budget reserves this per line so the FINAL rendered content stays
/// within [`MAX_READ_BYTES`].
const RENDER_PREFIX_CHARS: usize = 8;
/// The per-call capture budget for the rendered window text (the numbering
/// overhead is reserved up front).
const CAPTURE_CAP: usize = MAX_READ_BYTES - MAX_READ_LINES * RENDER_PREFIX_CHARS;

/// UTF-8 char-boundary test on raw bytes: a position is a boundary when the
/// byte there does not continue a multi-byte sequence (continuation bytes
/// are `0b10xxxxxx`). `str::is_char_boundary` is not available on `&[u8]`,
/// and the F2 cursor works in raw artifact bytes.
fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0 || index == bytes.len() || (bytes[index] & 0xC0) != 0x80
}
/// Per-call scan budget (W06). Producers cap captured logs at 8 MiB, so
/// every legally produced artifact is fully reachable; the scan streams —
/// it never materializes the file. A file beyond this budget is readable
/// in its earlier parts, and the report marks the totals as incomplete
/// instead of refusing the read.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;

pub struct ArtifactReadTool {
    workspace: Workspace,
}

impl ArtifactReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ArtifactReadArgs {
    reference: String,
    #[serde(default = "default_start_line")]
    start_line: usize,
    /// Absent means "one bounded page starting at `start_line`" — the same
    /// typed derivation on both sides (F1): the coverage footer only ever
    /// suggests `start_line`, so a suggested continuation must stay
    /// executable under the parser's own defaults. An explicit `end_line`
    /// keeps its exact old semantics.
    #[serde(default)]
    end_line: Option<usize>,
    /// F2: raw byte offset INSIDE `start_line` where the rendered window
    /// begins. A line longer than the capture cap is cut mid-line, and the
    /// coverage footer continues with `start_line` + `line_byte_offset` —
    /// the same typed cursor on both sides, bound to the artifact identity
    /// carried by `reference`. Only positions actually shown advance it.
    #[serde(default)]
    line_byte_offset: Option<usize>,
}

fn default_start_line() -> usize {
    1
}

/// Lines per derived (end_line-less) page. The old independent default was
/// `end_line = 200`; the derived window keeps that size, anchored at
/// `start_line` instead of at line 1.
const DEFAULT_PAGE_LINES: usize = 200;

#[async_trait]
impl Tool for ArtifactReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "artifact.read".into(),
            description: "Read lines from an artifact:// ref (spilled listings/logs).".into(),
            input_schema: json!({
                "type": "object",
                "required": ["reference"],
                "properties": {
                    "reference": {"type": "string", "description": "artifact:// reference from a previous tool result"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1, "description": "defaults to a bounded page starting at start_line"},
                    "line_byte_offset": {"type": "integer", "minimum": 0, "description": "raw byte offset inside start_line where the window begins (for lines longer than one page)"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
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
        let args: ArtifactReadArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("artifact.read args: {e}")))?;
        // F1: the parser and the continuation-footer generator share one
        // typed semantics — an absent end_line is a page anchored at
        // start_line (checked arithmetic: an unrepresentable window is
        // refused, never wrapped).
        let end_line = match args.end_line {
            Some(end_line) => end_line,
            None => args
                .start_line
                .checked_add(DEFAULT_PAGE_LINES - 1)
                .ok_or_else(|| {
                    AgentError::InvalidRequest(
                        "invalid line range: start_line leaves no room for a page".into(),
                    )
                })?,
        };
        let start_line = args.start_line;
        if start_line == 0 || end_line < start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        if end_line - start_line + 1 > MAX_READ_LINES {
            return Err(AgentError::InvalidRequest(format!(
                "artifact.read is limited to {MAX_READ_LINES} lines per call"
            )));
        }

        // The reference resolves to a cleaned relative path confined to the
        // run artifact store; the open itself goes through the pinned
        // directory-handle descent, so a link swap cannot redirect the read.
        let (_normalized, confined) = self
            .workspace
            .open_artifact_for_run(&args.reference, run_id)
            .await?;
        let display_path = confined.display().to_path_buf();

        // W06: stream the artifact line by line under a hard scan budget
        // instead of pre-reading the whole file. Any line range of a
        // legally produced artifact (producers cap captured output at
        // 8 MiB) is reachable; a byte-capped capture bounds the returned
        // window itself. Nothing trusts a size probe, and the report says
        // when the totals stopped at the scan budget.
        let file = confined.into_tokio();
        let mut reader = tokio::io::BufReader::new(file.take(MAX_SCAN_BYTES));
        // Reserve the per-line numbering overhead so the final rendered
        // content stays within MAX_READ_BYTES (W06 复核：声明预算约束的是
        // 实际返回字符串).
        let mut captured = String::new();
        let mut captured_bytes = 0usize;
        let mut captured_truncated = false;
        // F2: an in-line byte cursor. `Some((line, offset))` means the first
        // unshown position is INSIDE `line`: `offset` raw bytes of it have
        // been shown, the rest has not. Only actually shown positions
        // advance it, so a resumption never skips and never re-shows.
        let mut mid_line_cursor: Option<(usize, usize)> = None;
        let mut line_bytes = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut counted_lines = 0usize;
        let mut last_captured_line = 0usize;
        let start_offset = args.line_byte_offset.unwrap_or(0);
        loop {
            line_bytes.clear();
            let read = reader
                .read_until(b'\n', &mut line_bytes)
                .await
                .map_err(|e| AgentError::Io(format!("read artifact: {e}")))?;
            if read == 0 {
                break; // end of file (or of the scan budget)
            }
            scanned_bytes += read as u64;
            counted_lines += 1;
            if counted_lines < start_line || counted_lines > end_line {
                continue;
            }
            // Once a mid-line cursor exists, later window lines stay
            // unshown: the continuation must re-serve them in order — never
            // skip past the partial line, never show them twice.
            if mid_line_cursor.is_some() {
                continue;
            }
            // Line terminators are structure, not content: once every
            // content byte of a line is shown, only a cut EOL may remain,
            // and that is not an unshown region.
            let eol_len = if line_bytes.ends_with(b"\r\n") {
                2
            } else if line_bytes.ends_with(b"\n") {
                1
            } else {
                0
            };
            let content_len = line_bytes.len() - eol_len;
            let skip = if counted_lines == start_line {
                start_offset
            } else {
                0
            };
            if skip > 0 {
                if skip >= content_len {
                    return Err(AgentError::InvalidRequest(format!(
                        "line_byte_offset {skip} is at or past the end of line {start_line} ({content_len} content bytes)"
                    )));
                }
                if !is_char_boundary(&line_bytes, skip) {
                    return Err(AgentError::InvalidRequest(
                        "line_byte_offset must fall on a UTF-8 character boundary".into(),
                    ));
                }
            }
            let rest = &line_bytes[skip..];
            // The capture budget applies to the RENDERED text: raw bytes can
            // expand under lossy UTF-8 rendering (one invalid byte becomes a
            // three-byte replacement char), so budgeting raw bytes could
            // still overrun the cap.
            let rendered_full = String::from_utf8_lossy(rest);
            let room = CAPTURE_CAP.saturating_sub(captured_bytes);
            if room == 0 {
                // Capture budget exhausted: the remaining window lines
                // exist but are not shown. `last_captured_line` stays
                // behind so the paging cursor points at them.
            } else if rendered_full.len() <= room {
                captured_bytes += rendered_full.len();
                captured.push_str(&rendered_full);
                last_captured_line = counted_lines;
            } else {
                // W06 复核反例：截断必须消费预算、必须以行边界收尾。
                // F2: the cut is a RAW-byte cut on a char boundary and the
                // continuation cursor is tracked in raw bytes, so the shown
                // text and the resume position share one coordinate system.
                // Lossy expansion (invalid bytes) falls back to a third of
                // the room: one raw byte renders to at most three bytes, so
                // the fallback can never exceed the cap.
                let mut take = room.min(rest.len());
                while take > 0 && !is_char_boundary(rest, take) {
                    take -= 1;
                }
                let mut chunk = &rest[..take];
                if String::from_utf8_lossy(chunk).len() > room {
                    let mut fallback = room / 3;
                    while fallback > 0 && !is_char_boundary(rest, fallback) {
                        fallback -= 1;
                    }
                    chunk = &rest[..fallback];
                }
                if chunk.is_empty() {
                    // Not even one rendered byte fits the remaining room:
                    // the line stays unshown and the cursor stays behind.
                    continue;
                }
                let rendered = String::from_utf8_lossy(chunk);
                captured.push_str(&rendered);
                if !captured.ends_with('\n') {
                    captured.push('\n');
                }
                captured_bytes += rendered.len();
                last_captured_line = counted_lines;
                let shown_content = skip + chunk.len();
                if shown_content < content_len {
                    captured_truncated = true;
                    mid_line_cursor = Some((counted_lines, shown_content));
                }
                // Otherwise only the line terminator was cut: every content
                // byte is shown, so there is no unshown region in the line.
            }
        }
        // `take` returns 0 reads both at true EOF and at the scan budget;
        // remaining budget distinguishes them.
        let scan_complete = reader.get_ref().limit() > 0;
        // The captured text is exactly the requested window (from
        // start_line), so the render is window-relative.
        let lines: Vec<&str> = captured.lines().collect();
        let selected = lines
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6} | {}", start_line + offset, line))
            .collect::<Vec<_>>()
            .join("\n");
        // The cursor must not hide unshown data: an unshown in-line suffix
        // (capture cap cut mid-line), lines captured-then-dropped from the
        // window (capture cap), and lines beyond the scan budget all leave
        // unshown regions, and `has_more` must own each of them — "end of
        // artifact" needs the real absence of any unshown region.
        let first_unshown_in_window = match mid_line_cursor {
            Some((line, _)) => line,
            None => (last_captured_line + 1).max(start_line),
        };
        let in_window_unshown = first_unshown_in_window <= end_line.min(counted_lines);
        let beyond_window = scan_complete && end_line < counted_lines;
        let has_more = !scan_complete || in_window_unshown || beyond_window;
        let (next_start_line, next_line_byte_offset) = if in_window_unshown {
            mid_line_cursor.unwrap_or((first_unshown_in_window, 0))
        } else if has_more {
            (end_line.saturating_add(1), 0)
        } else {
            (end_line, 0)
        };

        // The body-level coverage statement (F04/CORE-3): the model sees
        // only `model_content`, so a window that is not the whole artifact
        // must name its range, the total, and the continuation or end
        // marker there. A complete single-page read stays plain; a resumed
        // mid-line read is never mistaken for one.
        let whole_artifact_in_one_page = start_line == 1
            && end_line >= counted_lines
            && !captured_truncated
            && start_offset == 0;
        let mut clauses: Vec<String> = Vec::new();
        if has_more || !scan_complete || !whole_artifact_in_one_page {
            if lines.is_empty() {
                clauses.push(format!(
                    "no lines in the scanned prefix of {} lines{}",
                    counted_lines,
                    if scan_complete {
                        String::new()
                    } else {
                        " (scan budget reached; totals are incomplete)".to_string()
                    }
                ));
            } else if scan_complete {
                clauses.push(format!(
                    "showing lines {}-{} of {} total",
                    start_line,
                    start_line + lines.len().saturating_sub(1),
                    counted_lines
                ));
            } else {
                clauses.push(format!(
                    "showing lines {}-{} of at least {} total (scan budget reached; totals are incomplete)",
                    start_line,
                    start_line + lines.len().saturating_sub(1),
                    counted_lines
                ));
            }
            if has_more {
                if mid_line_cursor.is_some() {
                    clauses.push(format!(
                        "line {next_start_line} is truncated at the capture cap; its unshown suffix continues inside the line"
                    ));
                }
                clauses.push(if next_line_byte_offset > 0 {
                    format!(
                        "continue with artifact.read reference={} start_line={next_start_line} line_byte_offset={next_line_byte_offset}",
                        args.reference
                    )
                } else {
                    format!(
                        "continue with artifact.read reference={} start_line={next_start_line}",
                        args.reference
                    )
                });
            } else {
                clauses.push(format!("end of artifact ({counted_lines} lines)"));
            }
        }
        let coverage = coverage_footer(clauses);

        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "artifact.read".into(),
                ok: true,
                summary: format!(
                    "read lines {}-{} of {} ({} lines total{}{})",
                    start_line,
                    start_line + lines.len().saturating_sub(1),
                    display_relative(&self.workspace, &display_path),
                    counted_lines,
                    if scan_complete {
                        String::new()
                    } else {
                        format!("; scan stopped at the {MAX_SCAN_BYTES}-byte per-call budget, totals are incomplete")
                    },
                    if captured_truncated {
                        "; window truncated at the per-call capture cap".to_string()
                    } else {
                        String::new()
                    },
                ),
                model_content: with_coverage_footer(
                    if selected.is_empty() {
                        "no lines in range".to_string()
                    } else {
                        selected
                    },
                    coverage,
                ),
                artifact_ref: Some(args.reference),
                metadata: json!({
                    "total_lines": counted_lines,
                    "total_lines_complete": scan_complete,
                    "bytes": scanned_bytes,
                    "returned": lines.len(),
                    "has_more": has_more,
                    "next_start_line": next_start_line,
                    "next_line_byte_offset": next_line_byte_offset,
                    "window_truncated": captured_truncated,
                }),
            }
            .with_native_execution_facts(super::builtin_bound(false)),
        ))
    }
}

fn display_relative(workspace: &Workspace, path: &std::path::Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("artifact.read must return a plain value"),
        }
    }

    async fn tool_with_artifact() -> (ArtifactReadTool, tempfile::TempDir, RunId, String) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let reference = workspace
            .write_artifact(run_id, "grep", "txt", b"alpha\nbeta\ngamma\ndelta\n")
            .await
            .unwrap();
        (ArtifactReadTool::new(workspace), dir, run_id, reference)
    }

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "artifact.read".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn reads_a_bounded_range_with_paging_metadata() {
        let (tool, _dir, run_id, reference) = tool_with_artifact().await;

        // Default range is lines 1..=200: the whole 4-line artifact.
        let output = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        assert!(output.model_content.contains("alpha"));
        assert!(output.model_content.contains("delta"));
        assert_eq!(output.metadata["total_lines"], 4);
        assert_eq!(output.metadata["has_more"], false);

        // A narrow range reports the paging cursor for the next page.
        let output = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 2, "end_line": 3}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.model_content.contains("beta"));
        assert!(output.model_content.contains("gamma"));
        assert!(!output.model_content.contains("delta"));
        assert_eq!(output.metadata["has_more"], true);
        assert_eq!(output.metadata["next_start_line"], 4);
    }

    #[tokio::test]
    async fn refuses_invalid_ranges_and_non_artifact_references() {
        let (tool, _dir, run_id, reference) = tool_with_artifact().await;

        let bad_range = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 5, "end_line": 3}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(bad_range.is_err(), "end before start must be refused");

        let too_wide = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 1, "end_line": 401}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(too_wide.is_err(), "over-wide ranges must be refused");

        let not_artifact = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": "artifact://src/main.rs"}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(
            not_artifact.is_err(),
            "workspace files are not readable as artifacts"
        );

        let not_a_scheme = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"reference": "https://example.com/x"}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(not_a_scheme.is_err(), "foreign schemes must be refused");
    }

    #[tokio::test]
    async fn missing_artifact_is_a_clean_error() {
        let (tool, _dir, run_id, _reference) = tool_with_artifact().await;
        let output = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": format!(
                        "artifact://v1/{run_id}/grep/0000000000000000000000000000000000000000000000000000000000000000"
                    )}),
                )
                .call.arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(output.is_err(), "a missing artifact must error cleanly");
    }

    #[tokio::test]
    async fn refuses_another_runs_artifact() {
        let (tool, _dir, owner_run, reference) = tool_with_artifact().await;
        let other_run = RunId::new();

        let output = tool
            .execute(
                other_run,
                "c",
                request(other_run, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await;

        assert_ne!(owner_run, other_run);
        assert!(
            output.is_err(),
            "artifact refs are scoped to their owning run"
        );
    }

    /// W06: a legally produced large artifact (producers cap captured
    /// output at 8 MiB) must keep a bounded line-range recovery path. The
    /// old implementation pre-read 2 MiB and refused the whole call, so
    /// even the first lines of a 3 MB log were unreachable.
    #[tokio::test]
    async fn large_artifact_line_ranges_stay_reachable() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // 3,000,000 bytes, one 100-byte line each, exactly 30,000 lines.
        let line = format!("{}\n", "l".repeat(99));
        let repeats = 3_000_000usize / line.len();
        let body = line.repeat(repeats);
        assert!(body.len() >= 3_000_000);
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace);

        // The first page of a 3 MB artifact.
        let output = value(
            tool.execute(
                run_id,
                "c",
                request(run_id, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(
            output.ok,
            "the first page must not be refused by size: {output:?}"
        );
        assert!(output.model_content.contains("     1 | "));
        assert_eq!(output.metadata["total_lines"], repeats);
        assert_eq!(output.metadata["total_lines_complete"], true);
        assert_eq!(output.metadata["has_more"], true);

        // A deep range past the old 2 MiB pre-read limit.
        let deep_start = 25_000usize;
        let output = value(
            tool.execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": deep_start, "end_line": deep_start + 10}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "a deep range must be reachable: {output:?}");
        assert!(
            output
                .model_content
                .contains(&format!("{deep_start:>6} | ")),
            "the requested line numbers must appear: {}",
            output.model_content
        );
        assert_eq!(output.metadata["next_start_line"], deep_start + 11);
    }

    // -- F1 regression harness ------------------------------------------------

    /// Run one artifact.read call through the REAL trusted output broker,
    /// exactly as the kernel does before a ToolOutcome reaches the actor.
    /// Continuations are extracted from the brokered body: a pointer that
    /// dies at the broker layer is dead for the model too.
    async fn read_through_broker(
        tool: &ArtifactReadTool,
        broker: &agent_workspace::WorkspaceOutputBroker,
        run_id: RunId,
        args: Value,
    ) -> ToolOutput {
        let output = value(
            tool.execute(run_id, "c", args, None, CancellationToken::new())
                .await
                .expect("artifact.read must execute"),
        );
        use agent_contracts::OutputBroker as _;
        broker.bound(run_id, None, output).await
    }

    /// Extract the continuation arguments EXACTLY as the model-visible body
    /// states them. The test must never add a parameter the tool did not
    /// return: the red case for F1 is a suggested `start_line` that the
    /// tool's own default `end_line` rejects.
    fn continuation_args(content: &str) -> Value {
        let marker = "continue with artifact.read ";
        let at = content
            .find(marker)
            .unwrap_or_else(|| panic!("the page must offer a continuation in its body: {content}"));
        let clause = content[at + marker.len()..]
            .split(['\n', ';'])
            .next()
            .unwrap_or_default();
        let mut reference = None;
        let mut start_line = None;
        let mut line_byte_offset = None;
        for token in clause.split_whitespace() {
            if let Some(value) = token.strip_prefix("reference=") {
                reference = Some(value.to_string());
            } else if let Some(value) = token.strip_prefix("start_line=") {
                start_line = value.parse::<usize>().ok();
            } else if let Some(value) = token.strip_prefix("line_byte_offset=") {
                line_byte_offset = value.parse::<usize>().ok();
            }
        }
        let mut args = serde_json::json!({
            "reference": reference.expect("the continuation must name the reference"),
            "start_line": start_line.expect("the continuation must name start_line"),
        });
        if let Some(offset) = line_byte_offset {
            args["line_byte_offset"] = serde_json::json!(offset);
        }
        args
    }

    /// F1: a 450+ line artifact is ordinary legal content with default
    /// arguments. Following the tool's own continuation verbatim — page by
    /// page through the real broker — must reach the sentinel lines at the
    /// end without ever inventing a parameter the tool did not return.
    #[tokio::test]
    async fn the_returned_continuation_is_executable_verbatim_until_the_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let total = 520usize;
        let mut body = String::new();
        for line in 1..=total {
            if line == 500 {
                body.push_str("sentinel-line-500\n");
            } else if line == total {
                body.push_str("final-line-520\n");
            } else {
                body.push_str(&format!("filler-line-{line}\n"));
            }
        }
        let reference = workspace
            .write_artifact(run_id, "grep", "txt", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = serde_json::json!({ "reference": reference });
        let mut seen_sentinel = false;
        let mut seen_final = false;
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = read_through_broker(&tool, &broker, run_id, args.clone()).await;
            assert!(
                output.ok,
                "every returned continuation must execute: {:?}",
                output.summary
            );
            pages += 1;
            assert!(
                pages < 8,
                "520 lines at 200 per page must end within a few pages"
            );
            if output.model_content.contains("sentinel-line-500") {
                seen_sentinel = true;
            }
            if output.model_content.contains("final-line-520") {
                seen_final = true;
            }
            if output.model_content.contains("end of artifact") {
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the continuation cursor must advance monotonically"
            );
            previous = (start, offset);
        }
        assert!(
            seen_sentinel && seen_final,
            "following the returned continuations verbatim must reach the tail sentinel lines"
        );
        assert!(pages >= 3, "520 lines cannot fit the first 200-line page");
    }

    /// F1 boundaries: the derived default stays a bounded page from
    /// start_line, explicit end_line keeps its old semantics, a start past
    /// the end is an honest empty page (not a range error), overflow is
    /// checked, and repeating a read does not double-count.
    #[tokio::test]
    async fn defaulted_end_line_stays_bounded_and_rejects_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let body = (1..=520)
            .map(|line| format!("filler-line-{line}\n"))
            .collect::<String>();
        let reference = workspace
            .write_artifact(run_id, "grep", "txt", body.as_bytes())
            .await
            .unwrap();
        let single_reference = workspace
            .write_artifact(run_id, "grep", "txt", b"only-line\n")
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace);
        let read = |args| {
            let tool = &tool;
            async move {
                value(
                    tool.execute(run_id, "c", args, None, CancellationToken::new())
                        .await
                        .unwrap(),
                )
            }
        };

        // A bare start_line gets a bounded page (start_line..start_line+199),
        // not the stale global default of 200.
        let paged = read(serde_json::json!({"reference": reference, "start_line": 2})).await;
        assert!(
            paged
                .model_content
                .contains(&format!("{:>6} | filler-line-2", 2))
        );
        assert!(
            paged
                .model_content
                .contains(&format!("{:>6} | filler-line-201", 201))
        );
        assert!(!paged.model_content.contains("filler-line-202"));
        assert_eq!(paged.metadata["next_start_line"], 202);

        // start_line beyond the end is an honest empty page with a real end
        // marker, not "invalid line range".
        let past_end = read(serde_json::json!({"reference": reference, "start_line": 1000})).await;
        assert!(past_end.ok);
        assert!(past_end.model_content.contains("no lines in range"));
        assert!(
            past_end
                .model_content
                .contains("end of artifact (520 lines)")
        );
        assert_eq!(past_end.metadata["has_more"], false);

        // An extreme start_line is checked arithmetic: rejected cleanly, no
        // overflow, no panic.
        let overflow = tool
            .execute(
                run_id,
                "c",
                serde_json::json!({"reference": reference, "start_line": usize::MAX}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(
            overflow.is_err(),
            "an unrepresentable window must be refused"
        );

        // A single-line file with pure defaults stays a plain complete read.
        let single = read(serde_json::json!({"reference": single_reference})).await;
        assert!(single.model_content.contains("only-line"));
        assert!(
            !single.model_content.contains("[coverage]"),
            "a complete single-page read stays plain: {}",
            single.model_content
        );

        // Repeating the same read does not double-count the lines.
        let first =
            read(serde_json::json!({"reference": reference, "start_line": 5, "end_line": 9})).await;
        let second =
            read(serde_json::json!({"reference": reference, "start_line": 5, "end_line": 9})).await;
        assert_eq!(first.metadata["total_lines"], 520);
        assert_eq!(second.metadata["total_lines"], 520);
    }

    /// W06 复核反例（F2 重写）：一条 3 MiB 的长首行＋100 行尾部。截断必须
    /// 消费捕获预算、以行边界收尾（不得把截断尾与 tail-0 拼接成一行）、返
    /// 回字符串不得超出声明的捕获预算；且续读游标必须指向第一个未展示的
    /// 「位置」——首行内部的字节偏移——而不是直接跳到第二行。
    #[tokio::test]
    async fn long_first_line_truncates_at_the_cap_without_merging_lines() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = "l".repeat(2 * 1024 * 1024);
        body.push_str("SUFFIX-SENTINEL");
        body.push_str(&"l".repeat(1024 * 1024 - 15));
        body.push('\n');
        for index in 0..100 {
            body.push_str(&format!("tail-{index}\n"));
        }
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace);

        let output = value(
            tool.execute(
                run_id,
                "c",
                request(run_id, json!({"reference": reference}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok);
        assert_eq!(output.metadata["total_lines"], 101);
        assert_eq!(output.metadata["total_lines_complete"], true);
        assert_eq!(output.metadata["window_truncated"], true);
        // The window renders as exactly one content line (the truncated
        // long line), plus the coverage footer — the truncated tail must
        // not merge with the next line, and the footer must not merge
        // with either (F04 页脚在正文自己的行上).
        let mut body_lines = output.model_content.lines();
        let content_line = body_lines.next().unwrap();
        assert!(
            content_line.starts_with("     1 | "),
            "the truncated long line is one rendered line: {content_line}"
        );
        assert_eq!(
            body_lines.count(),
            1,
            "exactly one content line plus one coverage footer remain: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("[coverage]"),
            "the truncated window must carry the coverage footer: {}",
            output.model_content
        );
        assert!(
            !output.model_content.contains("tail-0"),
            "the truncated tail must not merge with the next line"
        );
        assert!(
            output.model_content.len() <= MAX_READ_BYTES,
            "the returned string must stay within the declared capture cap: {}",
            output.model_content.len()
        );
        assert_eq!(output.metadata["has_more"], true);
        // F2: the cursor points INSIDE line 1 — at its unshown suffix —
        // not at line 2. Skipping to line 2 would silently drop the rest
        // of the first line.
        assert_eq!(
            output.metadata["next_start_line"], 1,
            "the cursor must stay on the partially shown line"
        );
        let offset = output.metadata["next_line_byte_offset"].as_u64().unwrap() as usize;
        assert!(
            offset > 0 && offset < 3 * 1024 * 1024,
            "the in-line offset must name the shown prefix: {offset}"
        );

        // Following the returned continuation verbatim shows the rest of
        // line 1 (with the sentinel) first, then the tail lines untouched.
        let page2 = value(
            tool.execute(
                run_id,
                "c",
                json!({"reference": reference, "start_line": 1, "line_byte_offset": offset}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(
            page2.model_content.contains("SUFFIX-SENTINEL"),
            "the unshown suffix must be reachable: {}",
            page2.model_content
        );
        assert!(page2.model_content.contains("tail-0"));
        assert!(page2.model_content.contains("tail-99"));
        assert_eq!(page2.metadata["total_lines"], 101);
        assert_eq!(page2.metadata["window_truncated"], false);
        assert_eq!(page2.metadata["has_more"], false);
        assert!(page2.model_content.contains("end of artifact (101 lines)"));

        // An explicit start_line=2 remains legal: a deliberate skip, not a
        // silently imposed one.
        let explicit = value(
            tool.execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"reference": reference, "start_line": 2, "end_line": 200}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(explicit.model_content.contains("tail-0"));
        assert!(!explicit.model_content.contains("SUFFIX-SENTINEL"));
    }

    /// F2 红例：3 MiB 单行工件（仍在 8 MiB 扫描预算内）。sentinel 在被截断
    /// 行的后半部分——第一页之后。按返回的续读参数一路走（经真实 broker），
    /// 必须可达 sentinel，且结束必须是真实 EOF：has_more=false、没有未展示
    /// 区间。
    #[tokio::test]
    async fn a_truncated_long_line_is_resumable_to_real_eof_through_the_broker() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut line = "a".repeat(2 * 1024 * 1024);
        line.push_str("SECOND-HALF-SENTINEL");
        line.push_str(&"b".repeat(1024 * 1024 - 20));
        let reference = workspace
            .write_artifact(run_id, "process", "log", format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({ "reference": reference });
        let mut seen_sentinel = false;
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = read_through_broker(&tool, &broker, run_id, args.clone()).await;
            assert!(output.ok, "{:?}", output.summary);
            pages += 1;
            assert!(
                pages < 6,
                "a ~3 MiB line at ~2 MiB per page needs a few pages, then a real end"
            );
            if output.model_content.contains("SECOND-HALF-SENTINEL") {
                seen_sentinel = true;
            }
            if output.model_content.contains("end of artifact") {
                assert_eq!(
                    output.metadata["has_more"], false,
                    "the end marker must match the metadata"
                );
                assert_eq!(
                    output.metadata["window_truncated"], false,
                    "EOF means nothing is left unshown"
                );
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the in-line cursor must advance monotonically"
            );
            assert_eq!(
                start, 1,
                "the whole artifact is one line; the cursor stays inside it"
            );
            previous = (start, offset);
        }
        assert!(
            seen_sentinel,
            "the sentinel in the truncated suffix must be reachable via returned continuations"
        );
    }

    /// F2：长首行＋短次行。首行截断后，续读必须先展示首行未展示的尾部
    /// （sentinel 在那里），然后才是第二行；不能跳过。
    #[tokio::test]
    async fn the_truncated_first_line_suffix_is_shown_before_the_second_line() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut line = "a".repeat(2 * 1024 * 1024);
        line.push_str("SUFFIX-SENTINEL");
        line.push_str(&"b".repeat(100_000));
        let body = format!("{line}\ntail-0\n");
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({ "reference": reference });
        let mut seen_sentinel = false;
        let mut seen_tail = false;
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = read_through_broker(&tool, &broker, run_id, args.clone()).await;
            assert!(output.ok);
            pages += 1;
            assert!(pages < 6);
            if output.model_content.contains("SUFFIX-SENTINEL") {
                seen_sentinel = true;
            }
            if output.model_content.contains("tail-0") {
                seen_tail = true;
            }
            if output.model_content.contains("end of artifact") {
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the cursor must advance monotonically"
            );
            previous = (start, offset);
        }
        assert!(
            seen_sentinel && seen_tail,
            "the first line's unshown suffix must be shown before (not instead of) the second line"
        );
    }

    /// F2：CRLF。行终止符是结构不是内容；截断点的字节游标必须与 CRLF 共存，
    /// 首行耗尽后游标推进到次行，最终真实 EOF。
    #[tokio::test]
    async fn a_crlf_long_line_resumes_and_reaches_the_next_line() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut line = "a".repeat(2 * 1024 * 1024 + 7);
        line.push_str("CRLF-SUFFIX-SENTINEL\r\n");
        let body = format!("{line}crlf-tail\r\n");
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({ "reference": reference });
        let mut seen_sentinel = false;
        let mut seen_tail = false;
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = read_through_broker(&tool, &broker, run_id, args.clone()).await;
            assert!(output.ok);
            pages += 1;
            assert!(pages < 6);
            if output.model_content.contains("CRLF-SUFFIX-SENTINEL") {
                seen_sentinel = true;
            }
            if output.model_content.contains("crlf-tail") {
                seen_tail = true;
            }
            if output.model_content.contains("end of artifact") {
                assert_eq!(output.metadata["total_lines"], 2);
                assert_eq!(output.metadata["has_more"], false);
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the cursor must advance monotonically"
            );
            previous = (start, offset);
        }
        assert!(
            seen_sentinel && seen_tail,
            "CRLF line must resume past its truncation point and reach the next line"
        );
    }

    /// F2：UTF-8 多字节在截断点附近。切分不得落在码点中间（任何一页都不得
    /// 出现 U+FFFD），原始字节偏移单调推进，sentinel 可达，结束真实。
    #[tokio::test]
    async fn a_multibyte_line_resumes_on_character_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // "界" is 3 bytes: 700_000 chars = 2_100_000 bytes (just past the
        // ~2 MiB capture cut), then the sentinel, then ~0.9 MB more.
        let mut line = "界".repeat(700_000);
        line.push_str("边界-SENTINEL-边界");
        line.push_str(&"界".repeat(300_000));
        let reference = workspace
            .write_artifact(run_id, "process", "log", format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let mut args = json!({ "reference": reference });
        let mut seen_sentinel = false;
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = read_through_broker(&tool, &broker, run_id, args.clone()).await;
            assert!(output.ok);
            assert!(
                !output.model_content.contains('\u{FFFD}'),
                "no page may cut a code point in half: {}",
                output.summary
            );
            pages += 1;
            assert!(pages < 6);
            if output.model_content.contains("边界-SENTINEL-边界") {
                seen_sentinel = true;
            }
            if output.model_content.contains("end of artifact") {
                assert_eq!(output.metadata["window_truncated"], false);
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the raw byte cursor must advance"
            );
            previous = (start, offset);
        }
        assert!(
            seen_sentinel,
            "the multibyte sentinel must survive resumption"
        );
    }

    /// F2：字节上限边界。内容字节恰好等于捕获预算：只有行终止符被切，没有
    /// 未展示内容——不得发明续读；多一个字节：存在真实未展示后缀——必须有
    /// 指向行内的游标，且走一步到达真实 EOF。
    #[tokio::test]
    async fn the_byte_cap_boundary_does_not_invent_a_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let capture_cap = MAX_READ_BYTES - MAX_READ_LINES * RENDER_PREFIX_CHARS;
        let tool = ArtifactReadTool::new(workspace.clone());
        let read = |reference: String, extra: Value| {
            let tool = &tool;
            let mut args = extra;
            args["reference"] = json!(reference);
            async move {
                value(
                    tool.execute(run_id, "c", args, None, CancellationToken::new())
                        .await
                        .unwrap(),
                )
            }
        };

        // Exactly the cap: every content byte is shown.
        let exact: String = "c".repeat(capture_cap);
        let exact_reference = workspace
            .write_artifact(run_id, "process", "log", format!("{exact}\n").as_bytes())
            .await
            .unwrap();
        let exact_page = read(exact_reference, json!({})).await;
        assert_eq!(exact_page.metadata["window_truncated"], false);
        assert_eq!(exact_page.metadata["has_more"], false);
        assert!(
            !exact_page.model_content.contains("[coverage]"),
            "an exactly-cap complete read stays a plain single page: {}",
            exact_page.model_content.len()
        );

        // One content byte more: a real suffix remains.
        let over: String = "c".repeat(capture_cap + 1);
        let over_reference = workspace
            .write_artifact(run_id, "process", "log", format!("{over}\n").as_bytes())
            .await
            .unwrap();
        let first = read(over_reference.clone(), json!({})).await;
        assert_eq!(first.metadata["window_truncated"], true);
        assert_eq!(first.metadata["has_more"], true);
        assert_eq!(first.metadata["next_start_line"], 1);
        assert_eq!(first.metadata["next_line_byte_offset"], capture_cap);

        let last = read(
            over_reference,
            json!({"start_line": 1, "line_byte_offset": capture_cap}),
        )
        .await;
        assert_eq!(last.metadata["has_more"], false);
        assert_eq!(last.metadata["window_truncated"], false);
        assert!(last.model_content.contains("end of artifact (1 lines)"));
    }

    /// F2：无效 UTF-8 的 lossy 渲染会把一个原始字节放大成三个字节。截断
    /// 必须按渲染后的字节计预算（返回字符串不超声明上限），游标仍按原始
    /// 字节推进并到达真实 EOF。
    #[tokio::test]
    async fn invalid_utf8_expansion_stays_within_the_capture_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = vec![0xFFu8; 3 * 1024 * 1024];
        body.push(b'\n');
        let reference = workspace
            .write_artifact(run_id, "process", "log", &body)
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace);

        let mut args = json!({ "reference": reference });
        let mut previous = (0usize, 0usize);
        let mut pages = 0usize;
        loop {
            let output = value(
                tool.execute(run_id, "c", args.clone(), None, CancellationToken::new())
                    .await
                    .unwrap(),
            );
            assert!(output.ok);
            assert!(
                output.model_content.len() <= MAX_READ_BYTES,
                "the rendered page must stay within the declared capture cap: {}",
                output.model_content.len()
            );
            pages += 1;
            assert!(pages < 8);
            if output.model_content.contains("end of artifact") {
                assert_eq!(output.metadata["window_truncated"], false);
                assert_eq!(output.metadata["has_more"], false);
                break;
            }
            args = continuation_args(&output.model_content);
            let start = args["start_line"].as_u64().unwrap() as usize;
            let offset = args
                .get("line_byte_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            assert!(
                (start, offset) > previous,
                "the raw byte cursor must advance"
            );
            previous = (start, offset);
        }
    }

    /// 扫描预算截断的工件：正文必须声明总数不完整，不能让模型把
    /// 「读到了预算处」当成「读完了」。
    #[tokio::test]
    async fn a_scan_budget_truncated_artifact_declares_incomplete_totals_in_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // 8 MiB 扫描预算之外再多几行，扫描必然停在预算处。
        let body = "x\n".repeat((MAX_SCAN_BYTES / 2) as usize + 16);
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace);

        let output = value(
            tool.execute(
                run_id,
                "c",
                json!({"reference": reference}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok);
        assert_eq!(output.metadata["total_lines_complete"], false);
        assert!(
            output.model_content.contains("totals are incomplete"),
            "the body must say the totals stopped at the scan budget: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("continue with artifact.read"),
            "the next page pointer stays reachable: {}",
            output.model_content
        );
    }
}

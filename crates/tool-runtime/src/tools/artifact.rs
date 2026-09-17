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
//!
//! Pages are captured under the FINAL model-content budget (the same cap
//! the trusted output broker enforces), envelope included, so a page
//! reaches the model verbatim and the continuation cursor is recomputed
//! against what was actually delivered — never against an internal
//! capture position the broker might have trimmed away (G2/G3).

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::{
    Tool, coverage_footer,
    page::{
        DeliveredSpan, FINAL_BODY_CHARS, finalize_within_budget, render_numbered_spans,
        rendered_prefix_chars,
    },
};

/// Per-call line-count cap: a page never spans more than this many source
/// lines regardless of budget.
const MAX_READ_LINES: usize = 400;

/// A position in the artifact source: a 1-based source line plus a raw
/// byte offset inside that line's content (0 = the line's first content
/// byte). Raw bytes, not rendered chars: this is the F2 continuation
/// cursor's coordinate system — the artifact's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SourcePosition {
    line: usize,
    line_byte_offset: usize,
}

impl SourcePosition {
    fn new(line: usize, line_byte_offset: usize) -> Self {
        Self {
            line,
            line_byte_offset,
        }
    }
}

/// G2: the three positions of one `artifact.read` call are distinct facts
/// and are carried in distinct fields — conflating them is exactly how a
/// continuation can advance past content the model never received:
///
/// - **scanned** ([`ScannedPosition`]): how far the line stream was
///   consumed. Owns the totals and the scan-budget honesty; it is never
///   a delivery claim.
/// - **captured** (`SourcePosition`, derived from the capture loop's
///   spans before finalization): the first position the capture loop did
///   not append under the delivered page budget.
/// - **delivered** (`CoverageFacts::next`, derived from the FINAL body in
///   [`finalize_delivered_page`]): the first source position the
///   model-visible body does not show. The continuation cursor comes
///   from here and only here.
#[derive(Debug, Clone, Copy)]
struct ScannedPosition {
    lines: usize,
    bytes: u64,
    complete: bool,
}

/// Typed facts the coverage footer is generated from. The SAME clause
/// builder ([`coverage_clauses`]) renders both the real footer and the
/// worst-case reservation probe, so the reserved footer budget cannot
/// drift from the footer a page actually appends. Delivered source
/// intervals live in the shared paging module
/// ([`super::page::DeliveredSpan`]); artifact reads cut lines mid-content
/// and track the cut in raw bytes via `shown_to`.
#[derive(Debug, Clone)]
struct CoverageFacts {
    /// First/last delivered source line; 0 = none delivered.
    first_line: usize,
    last_line: usize,
    counted_lines: usize,
    scan_complete: bool,
    /// The delivered page cut a line mid-content; its number goes in the
    /// footer clause.
    truncated_line: Option<usize>,
    has_more: bool,
    window_truncated: bool,
    next: SourcePosition,
    reference: String,
    /// A complete single-page read of the whole artifact stays plain (no
    /// footer).
    plain: bool,
}

/// The coverage footer clauses (F04/CORE-3), generated only from typed
/// facts — never parsed back from prose.
fn coverage_clauses(facts: &CoverageFacts) -> Vec<String> {
    let mut clauses = Vec::new();
    if facts.plain {
        return clauses;
    }
    if facts.first_line == 0 {
        clauses.push(format!(
            "no lines in the scanned prefix of {} lines{}",
            facts.counted_lines,
            if facts.scan_complete {
                String::new()
            } else {
                " (scan budget reached; totals are incomplete)".to_string()
            }
        ));
    } else if facts.scan_complete {
        clauses.push(format!(
            "showing lines {}-{} of {} total",
            facts.first_line, facts.last_line, facts.counted_lines
        ));
    } else {
        clauses.push(format!(
            "showing lines {}-{} of at least {} total (scan budget reached; totals are incomplete)",
            facts.first_line, facts.last_line, facts.counted_lines
        ));
    }
    if facts.has_more {
        if let Some(line) = facts.truncated_line {
            clauses.push(format!(
                "line {line} is cut at the page budget; its unshown suffix continues inside the line"
            ));
        }
        clauses.push(if facts.next.line_byte_offset > 0 {
            format!(
                "continue with artifact.read reference={} start_line={} line_byte_offset={}",
                facts.reference, facts.next.line, facts.next.line_byte_offset
            )
        } else {
            format!(
                "continue with artifact.read reference={} start_line={}",
                facts.reference, facts.next.line
            )
        });
    } else {
        clauses.push(format!("end of artifact ({} lines)", facts.counted_lines));
    }
    clauses
}

/// The worst-case footer THIS call can append: the same clause builder
/// with maximal numbers and the real reference. Its char length (plus the
/// '\n' that attaches the footer) is reserved out of the page budget, so
/// the final body always fits [`FINAL_BODY_CHARS`] and the broker's
/// trimmer never has to cut a page's middle.
fn worst_case_footer_chars(reference: &str) -> usize {
    let facts = CoverageFacts {
        first_line: usize::MAX,
        last_line: usize::MAX,
        counted_lines: usize::MAX,
        scan_complete: false,
        truncated_line: Some(usize::MAX),
        has_more: true,
        window_truncated: true,
        next: SourcePosition::new(usize::MAX, usize::MAX),
        reference: reference.to_string(),
        plain: false,
    };
    coverage_footer(coverage_clauses(&facts))
        .map(|footer| footer.chars().count() + 1)
        .unwrap_or(0)
}

/// The per-page budget for rendered content (numbering prefixes and
/// separators are accounted per span on top of it): the final body budget
/// minus the worst-case footer reservation for THIS reference.
fn page_content_budget_chars(reference: &str) -> usize {
    FINAL_BODY_CHARS.saturating_sub(worst_case_footer_chars(reference))
}

/// The first position the delivered spans do NOT cover. Because capture
/// closes at the first unshowable position (G3), the spans are contiguous
/// from the window start, so this needs no per-span map: an incomplete
/// last span resumes inside its line, anything else resumes at the line
/// after the last delivered one.
fn first_unshown_from_spans(
    spans: &[DeliveredSpan],
    window_start: SourcePosition,
) -> SourcePosition {
    match spans.last() {
        Some(span) if !span.complete => SourcePosition::new(span.line, span.shown_to),
        Some(span) => SourcePosition::new(span.line.saturating_add(1), 0),
        None => window_start,
    }
}

/// Derive the coverage facts (including the continuation cursor) from the
/// delivered spans and the scan totals.
fn coverage_facts(
    spans: &[DeliveredSpan],
    scanned: ScannedPosition,
    window_start: SourcePosition,
    end_line: usize,
    reference: &str,
) -> CoverageFacts {
    let first_line = spans.first().map_or(0, |span| span.line);
    let last_line = spans.last().map_or(0, |span| span.line);
    let truncated_line = spans
        .last()
        .filter(|span| !span.complete)
        .map(|span| span.line);
    let first_unshown = first_unshown_from_spans(spans, window_start);
    let in_window_unshown = first_unshown.line <= end_line.min(scanned.lines);
    let beyond_window = scanned.complete && end_line < scanned.lines;
    let has_more = !scanned.complete || in_window_unshown || beyond_window;
    let next = if in_window_unshown {
        first_unshown
    } else if has_more {
        SourcePosition::new(end_line.saturating_add(1), 0)
    } else {
        SourcePosition::new(end_line, 0)
    };
    let plain = scanned.complete
        && !has_more
        && window_start.line == 1
        && window_start.line_byte_offset == 0;
    CoverageFacts {
        first_line,
        last_line,
        counted_lines: scanned.lines,
        scan_complete: scanned.complete,
        truncated_line,
        has_more,
        window_truncated: in_window_unshown,
        next,
        reference: reference.to_string(),
        plain,
    }
}

/// The finalized artifact page: the FINAL body plus the coverage facts
/// derived from the kept spans — the facts (and the continuation cursor
/// inside them) always describe what the final body actually contains.
struct DeliveredPage {
    body: String,
    facts: CoverageFacts,
    /// How many source spans the final body contains.
    returned: usize,
}

/// The artifact-side entry into the shared "continuation respects the
/// delivered position" rule ([`super::page::finalize_within_budget`]).
/// Renders the final model body (numbered spans plus the coverage footer
/// generated from the same spans), measures it against
/// [`FINAL_BODY_CHARS`], and lets the shared loop drop trailing spans on
/// an overrun — then derives the coverage facts (including the
/// continuation cursor) from the kept spans only, so the cursor can never
/// name a source position the model did not receive.
fn finalize_delivered_page(
    spans: Vec<DeliveredSpan>,
    scanned: ScannedPosition,
    window_start: SourcePosition,
    end_line: usize,
    reference: &str,
) -> DeliveredPage {
    let page = finalize_within_budget(spans, |spans| {
        let facts = coverage_facts(spans, scanned, window_start, end_line, reference);
        let footer = coverage_footer(coverage_clauses(&facts));
        let mut body = render_numbered_spans(spans);
        if body.is_empty() {
            body.push_str("no lines in range");
        }
        (body, footer)
    });
    let facts = coverage_facts(&page.spans, scanned, window_start, end_line, reference);
    DeliveredPage {
        returned: page.spans.len(),
        body: page.body,
        facts,
    }
}

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
            // G2: the declared budget IS the page budget — the broker
            // clamps model_content to exactly the number the capture loop
            // pages under, from one definition.
            output_budget: Some(FINAL_BODY_CHARS),
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
        // 8 MiB) is reachable; the page budget bounds the returned window
        // itself. Nothing trusts a size probe, and the report says when
        // the totals stopped at the scan budget.
        let file = confined.into_tokio();
        let mut reader = tokio::io::BufReader::new(file.take(MAX_SCAN_BYTES));
        // G2: the page is captured under the FINAL delivered budget — the
        // same number the trusted broker clamps this tool to — with each
        // span's numbering prefix, its separator, and a worst-case
        // coverage footer reserved up front. A page that fits here reaches
        // the model verbatim, so the broker's head+tail preview never cuts
        // a page's middle and the capture cursor cannot outrun the
        // delivered cursor.
        let content_budget = page_content_budget_chars(&args.reference);
        let mut spans: Vec<DeliveredSpan> = Vec::new();
        let mut used_chars = 0usize;
        // G3: capture closes at the FIRST position that cannot be shown.
        // Later window lines stay unshown (the scan continues so the
        // totals stay honest) and the cursor keeps exactly that position —
        // a later shorter line is never accepted across the hole.
        let mut capture_closed = false;
        let mut line_bytes = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut counted_lines = 0usize;
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
            if capture_closed {
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
            let rest = &line_bytes[skip..content_len];
            // The span's envelope: its ACTUAL numbering prefix width plus
            // one separator. The budget counts rendered CHARS — the broker
            // trims by chars, and a byte budget could let a multibyte page
            // overshoot it.
            let envelope = rendered_prefix_chars(counted_lines) + 1;
            let room = content_budget.saturating_sub(used_chars + envelope);
            let span = if rest.len() <= room {
                // Whole line. Lossy-rendered chars never exceed raw bytes,
                // so a line that fits by bytes fits by chars too.
                let mut text = String::from_utf8_lossy(rest).into_owned();
                if text.ends_with('\r') {
                    text.pop();
                }
                DeliveredSpan {
                    line: counted_lines,
                    shown_to: content_len,
                    complete: true,
                    text,
                }
            } else {
                // F2: the cut is a RAW-byte cut on a char boundary and the
                // cursor stays in the artifact's coordinate system. `room`
                // counts rendered chars and every char is at least one raw
                // byte, so taking `room` raw bytes can never overrun the
                // char budget (lossy expansion renders one raw byte as at
                // most one U+FFFD char).
                let mut take = room.min(rest.len());
                while take > 0 && !is_char_boundary(rest, take) {
                    take -= 1;
                }
                if take == 0 {
                    // G3: not even the first character of this line fits
                    // the leftover budget. Capture closes HERE.
                    capture_closed = true;
                    continue;
                }
                let chunk = &rest[..take];
                let mut text = String::from_utf8_lossy(chunk).into_owned();
                if text.ends_with('\r') {
                    text.pop();
                }
                DeliveredSpan {
                    line: counted_lines,
                    shown_to: skip + take,
                    complete: false,
                    text,
                }
            };
            let span_complete = span.complete;
            used_chars += envelope + span.text.chars().count();
            spans.push(span);
            if !span_complete {
                // The line's unshown suffix is the first unshowable
                // position: nothing later in the window may be captured.
                capture_closed = true;
            }
        }
        // `take` returns 0 reads both at true EOF and at the scan budget;
        // remaining budget distinguishes them.
        let scan_complete = reader.get_ref().limit() > 0;
        let scanned = ScannedPosition {
            lines: counted_lines,
            bytes: scanned_bytes,
            complete: scan_complete,
        };
        let window_start = SourcePosition::new(start_line, start_offset);
        // The captured position: the first unshown position per the
        // capture loop itself, before envelope finalization.
        let captured = first_unshown_from_spans(&spans, window_start);
        // The delivered position: derived from the FINAL body in one
        // trusted exit — this is the only source of the continuation.
        let page = finalize_delivered_page(spans, scanned, window_start, end_line, &args.reference);
        let facts = page.facts;
        let delivered = facts.next;

        let body_first = if facts.first_line > 0 {
            facts.first_line
        } else {
            start_line
        };
        let body_last = if facts.last_line > 0 {
            facts.last_line
        } else {
            start_line
        };
        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "artifact.read".into(),
                ok: true,
                summary: format!(
                    "read lines {}-{} of {} ({} lines total{}{})",
                    body_first,
                    body_last,
                    display_relative(&self.workspace, &display_path),
                    scanned.lines,
                    if scanned.complete {
                        String::new()
                    } else {
                        format!("; scan stopped at the {MAX_SCAN_BYTES}-byte per-call budget, totals are incomplete")
                    },
                    if facts.window_truncated {
                        "; window truncated at the per-call page budget".to_string()
                    } else {
                        String::new()
                    },
                ),
                model_content: page.body,
                artifact_ref: Some(args.reference.clone()),
                metadata: json!({
                    "total_lines": scanned.lines,
                    "total_lines_complete": scanned.complete,
                    "bytes": scanned.bytes,
                    "returned": page.returned,
                    "has_more": facts.has_more,
                    "next_start_line": delivered.line,
                    "next_line_byte_offset": delivered.line_byte_offset,
                    "window_truncated": facts.window_truncated,
                    // G2: the three positions, reported separately —
                    // scanned (totals, never a delivery claim), captured
                    // (capture loop), delivered (final body; owns the
                    // continuation above).
                    "positions": {
                        "scanned": {
                            "lines": scanned.lines,
                            "bytes": scanned.bytes,
                            "complete": scanned.complete,
                        },
                        "captured": {
                            "line": captured.line,
                            "line_byte_offset": captured.line_byte_offset,
                        },
                        "delivered": {
                            "line": delivered.line,
                            "line_byte_offset": delivered.line_byte_offset,
                        },
                    },
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
    /// exactly as the kernel does before a ToolOutcome reaches the actor
    /// (the kernel passes the executed tool's declared output budget).
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
        broker.bound(run_id, Some(FINAL_BODY_CHARS), output).await
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
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

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
            output.model_content.chars().count() <= FINAL_BODY_CHARS,
            "the returned string must stay within the final model-content budget: {}",
            output.model_content.chars().count()
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
        // The three positions are reported separately and agree while the
        // envelope fits (no finalize fallback was needed).
        assert_eq!(
            output.metadata["positions"]["captured"], output.metadata["positions"]["delivered"],
            "capture and delivered positions coincide when the page fits the budget"
        );

        // Following the returned continuations verbatim shows the rest of
        // line 1 (with the sentinel) first, then the tail lines untouched,
        // and ends at the real EOF.
        let pages = walk_pages_to_end(
            &tool,
            &broker,
            run_id,
            continuation_args(&output.model_content),
            260,
        )
        .await;
        assert!(
            pages
                .iter()
                .any(|page| page.model_content.contains("SUFFIX-SENTINEL")),
            "the unshown suffix must be reachable via returned continuations"
        );
        let last = pages.last().unwrap();
        assert!(last.model_content.contains("tail-0"));
        assert!(last.model_content.contains("tail-99"));
        assert_eq!(last.metadata["total_lines"], 101);
        assert_eq!(last.metadata["window_truncated"], false);
        assert_eq!(last.metadata["has_more"], false);
        assert!(last.model_content.contains("end of artifact (101 lines)"));

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
                pages < 300,
                "a ~3 MiB line at one final-budget page per step converges, then a real end"
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
            assert!(pages < 220);
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
            assert!(pages < 220);
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
    /// 行长刻意跨多页（G2 后每页按最终 model-content 预算切）。
    #[tokio::test]
    async fn a_multibyte_line_resumes_on_character_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // "界" is 3 bytes: 40_000 chars = 120_000 bytes (several final-budget
        // pages), then the sentinel, then 30_000 chars more.
        let mut line = "界".repeat(40_000);
        line.push_str("边界-SENTINEL-边界");
        line.push_str(&"界".repeat(30_000));
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
            assert!(pages < 30);
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

    /// F2/G2 页预算边界。一行内容恰好装满本页预算：完整展示、无未展示内
    /// 容——不得发明续读；多一个字符：存在真实未展示后缀——必须有指向行内
    /// 的游标，且按返回的续读走一步到达真实 EOF。预算（页脚预留、编号前
    /// 缀、分隔符）用生产同一 helper 推导。
    #[tokio::test]
    async fn the_byte_cap_boundary_does_not_invent_a_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
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
        // The exact single-line capacity of one page under the FINAL
        // budget: page content budget minus the line's own prefix and
        // separator. The probe artifact is a small stand-in used only to
        // obtain the production budget for a REAL reference shape.
        let probe_reference = workspace
            .write_artifact(run_id, "process", "log", b"probe\n")
            .await
            .unwrap();
        let budget = page_content_budget_chars(&probe_reference);
        assert!(budget > 0 && budget < FINAL_BODY_CHARS);
        let exact_chars = budget - rendered_prefix_chars(1) - 1;

        // Exactly the capacity: every content char is shown, the page is
        // the whole artifact, so it stays plain — no invented continuation.
        let exact: String = "c".repeat(exact_chars);
        let exact_reference = workspace
            .write_artifact(run_id, "process", "log", format!("{exact}\n").as_bytes())
            .await
            .unwrap();
        let exact_page = read(exact_reference, json!({})).await;
        assert_eq!(exact_page.metadata["window_truncated"], false);
        assert_eq!(exact_page.metadata["has_more"], false);
        assert_eq!(exact_page.metadata["returned"], 1);
        assert!(
            !exact_page.model_content.contains("[coverage]"),
            "an exactly-budget complete read stays a plain single page: {}",
            exact_page.model_content.chars().count()
        );

        // One content char more: a real suffix remains.
        let over: String = "c".repeat(exact_chars + 1);
        let over_reference = workspace
            .write_artifact(run_id, "process", "log", format!("{over}\n").as_bytes())
            .await
            .unwrap();
        let first = read(over_reference.clone(), json!({})).await;
        assert_eq!(first.metadata["window_truncated"], true);
        assert_eq!(first.metadata["has_more"], true);
        assert_eq!(first.metadata["next_start_line"], 1);
        assert_eq!(first.metadata["next_line_byte_offset"], exact_chars);

        let last = read(
            over_reference,
            json!({"start_line": 1, "line_byte_offset": exact_chars}),
        )
        .await;
        assert_eq!(last.metadata["has_more"], false);
        assert_eq!(last.metadata["window_truncated"], false);
        assert!(last.model_content.contains("end of artifact (1 lines)"));
    }

    /// F2：无效 UTF-8 的 lossy 渲染把一个原始字节渲染成一个 U+FFFD 字符。
    /// 预算按渲染字符计（返回字符串不超最终 model-content 上限），游标仍按
    /// 原始字节推进并到达真实 EOF。
    #[tokio::test]
    async fn invalid_utf8_expansion_stays_within_the_capture_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = vec![0xFFu8; 256 * 1024];
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
                output.model_content.chars().count() <= FINAL_BODY_CHARS,
                "the rendered page must stay within the final model-content budget: {}",
                output.model_content.chars().count()
            );
            pages += 1;
            assert!(pages < 40);
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

    // -- G2/G3 regressions (tenth batch) --------------------------------------
    //
    // These probes consume ONLY what the product actually returns: the walk
    // follows the continuation printed in the model-visible body (after the
    // real broker), never inventing a parameter, and coverage is proven by
    // whole families of unique block IDs — one sentinel proving nothing.

    /// Walk an artifact to its real end through the REAL tool→broker path,
    /// following only the continuations the model-visible body offers.
    /// Every page must stay inside the final model-content budget; the
    /// cursor must advance monotonically; "end" must match has_more=false.
    async fn walk_pages_to_end(
        tool: &ArtifactReadTool,
        broker: &agent_workspace::WorkspaceOutputBroker,
        run_id: RunId,
        first_args: Value,
        max_pages: usize,
    ) -> Vec<ToolOutput> {
        let mut args = first_args;
        let mut pages = Vec::new();
        let mut previous = (0usize, 0usize);
        loop {
            let output = read_through_broker(tool, broker, run_id, args.clone()).await;
            assert!(
                output.ok,
                "every returned continuation must execute: {:?}",
                output.summary
            );
            assert!(
                output.model_content.chars().count()
                    <= agent_contracts::MAX_TOOL_MODEL_CONTENT_CHARS,
                "the FINAL model-visible body must stay within the model-content budget: {} chars",
                output.model_content.chars().count()
            );
            assert!(
                !output.model_content.contains("output broker truncated"),
                "a page under the final budget must reach the model verbatim — \
                 the broker's trimmer may never cut an artifact.read page: {}",
                output.summary
            );
            pages.push(output.clone());
            assert!(
                pages.len() < max_pages,
                "the walk must converge within {max_pages} pages"
            );
            if output.model_content.contains("end of artifact") {
                assert_eq!(
                    output.metadata["has_more"], false,
                    "the end marker must match the metadata: {:?}",
                    output.metadata
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
                "the continuation cursor must advance monotonically, got ({start}, {offset}) after {previous:?}"
            );
            previous = (start, offset);
        }
        pages
    }

    /// Parse a delivered page body into its rendered (source line, text)
    /// entries. The coverage footer line and non-rendered lines are
    /// excluded; probes control their content so no artifact line mimics
    /// the numbering prefix.
    fn rendered_entries(body: &str) -> Vec<(usize, String)> {
        body.lines()
            .filter_map(|line| {
                if line.starts_with('[') {
                    return None; // [coverage] footer, not a rendered entry
                }
                let trimmed = line.trim_start();
                let (number, text) = trimmed.split_once(" | ")?;
                Some((number.parse::<usize>().ok()?, text.to_string()))
            })
            .collect()
    }

    /// How many delivered page bodies contain the tag.
    fn pages_containing(pages: &[ToolOutput], tag: &str) -> usize {
        pages
            .iter()
            .filter(|page| page.model_content.contains(tag))
            .count()
    }

    /// G2 probe A (review §3, ordinary log): 500 lines of exactly 150
    /// content chars (~75.5 KB). Every line carries a unique block ID;
    /// lines 100 and 300 sit mid-page under the old 200-line pages.
    /// Following ONLY the returned continuations through the real broker
    /// must deliver every line exactly once — the union of delivered
    /// source intervals covers the artifact with no gaps and no overlaps —
    /// and "end" may only appear after that.
    #[tokio::test]
    async fn g2_probe_a_every_line_delivered_exactly_once_via_returned_continuations() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let total = 500usize;
        let line_ids: Vec<String> = (1..=total)
            .map(|line| format!("G2A-L{line:04}-{line:x}7f{line:03}"))
            .collect();
        let mut body = String::new();
        for line in 1..=total {
            let id = &line_ids[line - 1];
            let mut text = id.clone();
            while text.chars().count() < 150 {
                text.push('x');
            }
            body.push_str(&text);
            body.push('\n');
        }
        assert_eq!(body.len(), 500 * 151);
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let pages = walk_pages_to_end(
            &tool,
            &broker,
            run_id,
            json!({ "reference": reference }),
            40,
        )
        .await;

        // No gaps, no overlaps, verbatim: a page boundary may cut a line
        // mid-content, so each source line is RECONSTRUCTED from its
        // delivered spans (entries carry true source numbers; consecutive
        // entries with the same number are that line's shown spans in
        // order) and must equal the original 150-char content exactly —
        // delivered exactly once, never re-shown, never skipped.
        let mut delivered: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        for page in &pages {
            for (number, text) in rendered_entries(&page.model_content) {
                delivered.entry(number).or_default().push_str(&text);
            }
        }
        assert_eq!(
            delivered.keys().copied().collect::<Vec<_>>(),
            (1..=total).collect::<Vec<_>>(),
            "every source line must be delivered"
        );
        for (number, text) in &delivered {
            let mut expected = line_ids[number - 1].clone();
            while expected.chars().count() < 150 {
                expected.push('x');
            }
            assert_eq!(
                text, &expected,
                "line {number} must be delivered verbatim, exactly once"
            );
        }
        // The review's mid-page marker lines (100 and 300) are covered by
        // the equality above; assert them once more, explicitly.
        assert!(delivered[&100].contains(&line_ids[99]));
        assert!(delivered[&300].contains(&line_ids[299]));
    }

    /// G2 probe B (review §3, single 3 MiB line): unique block IDs at
    /// ~1 MiB, ~2 MiB, ~2.5 MiB and near the end. Internal pages sized by
    /// the old ~2 MiB capture cap get squeezed to a 16k head+tail preview
    /// by the broker, so the 1 MiB and 2.5 MiB IDs never reach the model
    /// even though the walk "ends". Following the returned continuations
    /// (the line_byte_offset path) must deliver every ID exactly once,
    /// with contiguous, non-overlapping byte intervals.
    #[tokio::test]
    async fn g2_probe_b_three_mib_single_line_block_ids_all_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mib = 1024 * 1024usize;
        let total = 3 * mib;
        let mut line = String::with_capacity(total);
        let push_to = |line: &mut String, at: usize, tag: &str| {
            assert!(at > line.len(), "block positions must be increasing");
            while line.len() < at {
                line.push('a');
            }
            line.push_str(tag);
        };
        push_to(&mut line, mib, "G2B-BLK-1M-c31f");
        push_to(&mut line, 2 * mib, "G2B-BLK-2M-8ad0");
        push_to(&mut line, 5 * mib / 2, "G2B-BLK-2_5M-e4b9");
        push_to(&mut line, total - 64, "G2B-BLK-END-77aa");
        while line.len() < total {
            line.push('a');
        }
        assert_eq!(line.len(), total);
        let reference = workspace
            .write_artifact(run_id, "process", "log", format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let pages = walk_pages_to_end(
            &tool,
            &broker,
            run_id,
            json!({ "reference": reference }),
            300,
        )
        .await;

        for tag in [
            "G2B-BLK-1M-c31f",
            "G2B-BLK-2M-8ad0",
            "G2B-BLK-2_5M-e4b9",
            "G2B-BLK-END-77aa",
        ] {
            assert_eq!(
                pages_containing(&pages, tag),
                1,
                "block {tag} must be delivered exactly once by the returned continuations"
            );
        }
        // Straddle-proof no-gap/no-overlap proof: reconstruct the line from
        // the delivered spans (a page cut may split a tag) — every block ID
        // appears exactly once in the reconstruction.
        let mut reconstructed = String::new();
        for page in &pages {
            for (_number, text) in rendered_entries(&page.model_content) {
                reconstructed.push_str(&text);
            }
        }
        for tag in [
            "G2B-BLK-1M-c31f",
            "G2B-BLK-2M-8ad0",
            "G2B-BLK-2_5M-e4b9",
            "G2B-BLK-END-77aa",
        ] {
            assert_eq!(
                reconstructed.matches(tag).count(),
                1,
                "block {tag} must appear exactly once in the delivered union"
            );
        }
        // The delivered byte intervals are contiguous and non-overlapping:
        // each page's rendered text is exactly the bytes between the
        // previous cursor and this page's cursor (ASCII line, so rendered
        // chars are raw bytes), and the walk ends precisely at the line
        // end — the union covers the artifact exactly.
        let mut prev_offset = 0usize;
        let mut delivered_end = 0usize;
        for page in &pages {
            let next_offset = page.metadata["next_line_byte_offset"].as_u64().unwrap() as usize;
            let entries = rendered_entries(&page.model_content);
            assert_eq!(entries.len(), 1, "a single-line artifact renders one entry");
            let shown = entries[0].1.chars().count();
            if page.model_content.contains("end of artifact") {
                // Final page: the cursor stops moving; the shown span runs
                // to the end of the line.
                assert_eq!(next_offset, 0, "the end page's cursor stops moving");
                delivered_end = prev_offset + shown;
            } else {
                assert_eq!(
                    prev_offset + shown,
                    next_offset,
                    "page delivered bytes [{prev_offset}, {}) exactly",
                    prev_offset + shown
                );
                prev_offset = next_offset;
                delivered_end = next_offset;
            }
        }
        assert_eq!(
            delivered_end, total,
            "the delivered intervals must cover the whole line exactly"
        );
    }

    /// G3 probe (review §4), legacy sizing: line 1 is sized against the
    /// OLD internal capture cap so that, on the old code, the remaining
    /// room is exactly 1 byte — smaller than the first char of line 2
    /// (the 3-byte 界) — while the one-char ASCII line after it DOES fit
    /// the leftover budget. The capture must stop at the FIRST unshowable
    /// position — the 界 line — so following the returned continuations
    /// delivers the 界 line (true number 2) and the short line (true
    /// number 3) with no hole. The old code skipped the 界 line, captured
    /// the short line after it, renumbered it to "line 2", and claimed
    /// end-of-artifact with has_more=false; under the delivered-budget
    /// paging the same body exercises the long-line resumption path with
    /// the same no-hole invariants.
    #[tokio::test]
    async fn g3_multibyte_gap_captures_no_line_after_the_first_unshowable_position() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // The ninth batch's internal capture cap (2 MiB minus the old
        // 8-char-per-line reservation) — the budget the review's original
        // counterexample was sized against.
        const LEGACY_CAPTURE_CAP: usize = 2 * 1024 * 1024 - 400 * 8;
        // Line 1 (with its newline) consumes LEGACY_CAPTURE_CAP - 1 bytes,
        // so on the old code the remaining room is 1 byte.
        let line1 = format!("{}\n", "a".repeat(LEGACY_CAPTURE_CAP - 2));
        assert_eq!(line1.len(), LEGACY_CAPTURE_CAP - 1);
        let body = format!("{line1}界G3-MID-MARKER\nq\n");
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        // Page 1: the 界 line's first char does not fit the leftover room,
        // so line 2 is the FIRST unshowable position. The page must say
        // content remains (has_more) and must not capture any line after
        // that position — the old code captured the following short line
        // and renumbered it to "line 2".
        let first =
            read_through_broker(&tool, &broker, run_id, json!({ "reference": reference })).await;
        assert_eq!(
            first.metadata["has_more"], true,
            "line 2 is unshown: the page must not claim completion"
        );
        assert!(
            !first.model_content.contains("G3-MID-MARKER"),
            "the 界 line cannot have been delivered yet"
        );
        assert!(
            rendered_entries(&first.model_content)
                .iter()
                .all(|(number, _)| *number == 1),
            "no line after the first unshowable position may be captured: {}",
            first.model_content
        );

        // Following ONLY the returned continuations must deliver the 界
        // line (true number 2) and the short line (true number 3) — no
        // hole — and end at the real EOF of the 3-line artifact.
        let mut pages = vec![first];
        pages.extend(
            walk_pages_to_end(
                &tool,
                &broker,
                run_id,
                continuation_args(&pages[0].model_content),
                260,
            )
            .await,
        );

        assert_eq!(
            pages_containing(&pages, "G3-MID-MARKER"),
            1,
            "the 界 line must be delivered, not skipped"
        );
        let mut saw_mid = false;
        let mut saw_short = false;
        for page in &pages {
            for (number, text) in rendered_entries(&page.model_content) {
                if text.contains("G3-MID-MARKER") {
                    assert_eq!(number, 2, "the 界 line is source line 2");
                    saw_mid = true;
                }
                if number == 3 {
                    assert_eq!(text, "q", "source line 3 is the short ASCII line");
                    saw_short = true;
                }
            }
        }
        assert!(saw_mid, "the 界 line must be rendered");
        assert!(saw_short, "the short ASCII line must be rendered as line 3");
        assert!(
            pages
                .last()
                .unwrap()
                .model_content
                .contains("end of artifact (3 lines)"),
            "the walk must end at the real EOF of the 3-line artifact"
        );
    }

    /// G3 exact-room probe: sized against the CURRENT production page
    /// budget (derived with the same helpers the capture loop uses), so
    /// after line 1 exactly ONE char of room remains for line 2's text;
    /// line 2 starts with the 3-byte 界 and the following short ASCII
    /// line WOULD fit that leftover. Capture must close at the first
    /// unshowable position — (line 2, byte 0) — the page must report that
    /// exact position as the delivered continuation, and no later line
    /// may be captured across the hole.
    #[tokio::test]
    async fn g3_exact_room_closes_capture_at_the_unshowable_line_start() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        // A real reference of the production shape, to derive the same
        // per-call budget the capture loop uses (the footer reservation
        // embeds the reference, and every artifact reference of this run
        // has the same length).
        let probe_reference = workspace
            .write_artifact(run_id, "process", "log", b"probe\n")
            .await
            .unwrap();
        let budget = page_content_budget_chars(&probe_reference);
        assert!(budget > 100, "the page budget must be usable: {budget}");
        // Line 1's chars leave exactly one char of room after line 1's
        // prefix+separator and line 2's prefix+separator are reserved.
        let line1_len = budget - rendered_prefix_chars(1) - 1 - rendered_prefix_chars(2) - 1 - 1;
        let body = format!("{}\n界XG3R-MARKER\nqq\n", "a".repeat(line1_len));
        let reference = workspace
            .write_artifact(run_id, "process", "log", body.as_bytes())
            .await
            .unwrap();
        let tool = ArtifactReadTool::new(workspace.clone());
        let broker = agent_workspace::WorkspaceOutputBroker::new(std::sync::Arc::new(workspace));

        let first =
            read_through_broker(&tool, &broker, run_id, json!({ "reference": reference })).await;
        assert_eq!(
            first.metadata["has_more"], true,
            "line 2 is unshown: the page must not claim completion"
        );
        assert_eq!(first.metadata["window_truncated"], true);
        assert_eq!(first.metadata["returned"], 1);
        // The continuation is exactly the first unshowable position: line
        // 2, its first byte — both reported positions agree.
        assert_eq!(first.metadata["next_start_line"], 2);
        assert_eq!(first.metadata["next_line_byte_offset"], 0);
        assert_eq!(
            first.metadata["positions"]["captured"],
            json!({"line": 2, "line_byte_offset": 0})
        );
        assert_eq!(
            first.metadata["positions"]["delivered"],
            json!({"line": 2, "line_byte_offset": 0})
        );
        let entries = rendered_entries(&first.model_content);
        assert_eq!(entries.len(), 1, "only line 1 may be delivered");
        assert_eq!(entries[0].0, 1);
        assert!(
            !first.model_content.contains("XG3R-MARKER") && !first.model_content.contains("qq"),
            "no line after the first unshowable position may be captured: {}",
            first.model_content
        );

        // Follow the returned continuation verbatim: line 2 then line 3
        // under their TRUE source numbers, ending at the real EOF.
        let second = read_through_broker(
            &tool,
            &broker,
            run_id,
            continuation_args(&first.model_content),
        )
        .await;
        let entries = rendered_entries(&second.model_content);
        assert_eq!(
            entries
                .iter()
                .map(|(number, _)| *number)
                .collect::<Vec<_>>(),
            vec![2, 3],
            "source line identity is preserved across the hole: {}",
            second.model_content
        );
        assert!(entries[0].1.contains("XG3R-MARKER"));
        assert_eq!(entries[1].1, "qq");
        assert_eq!(second.metadata["has_more"], false);
        assert!(second.model_content.contains("end of artifact (3 lines)"));
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

    /// G2 safety net for the single maintenance entry: if the envelope
    /// ever overran the final budget (footer reserve drift, prefix-width
    /// surprise), `finalize_delivered_page` must drop trailing spans until
    /// the body fits and pull the delivered cursor back to the first
    /// dropped position — never past content the model did not receive.
    /// Driven directly on the internals: the capture loop's own accounting
    /// keeps this path from triggering through the public interface.
    #[test]
    fn finalize_falls_back_behind_the_overrunning_spans() {
        let scanned = ScannedPosition {
            lines: 500,
            bytes: 75_500,
            complete: true,
        };
        let window_start = SourcePosition::new(1, 0);
        // Spans that (illegally) overrun the budget: 200 lines of 200
        // chars each — roughly 41k body chars against a ~15.5k page.
        let spans: Vec<DeliveredSpan> = (1..=200)
            .map(|line| DeliveredSpan {
                line,
                shown_to: 200,
                complete: true,
                text: "x".repeat(200),
            })
            .collect();
        let reference = "artifact://v1/00000000-0000-0000-0000-000000000000/process/log/\
                         0000000000000000000000000000000000000000000000000000000000000000";
        let page = finalize_delivered_page(spans, scanned, window_start, 200, reference);
        // The final body fits the final budget, footer included.
        assert!(
            page.body.chars().count() <= FINAL_BODY_CHARS,
            "the finalized body must fit: {}",
            page.body.chars().count()
        );
        // The delivered cursor names the first position the FINAL body
        // does not show: the line after the last kept span.
        let kept = page.returned;
        assert!(kept < 200, "the overrunning spans must be dropped");
        assert_eq!(page.facts.next, SourcePosition::new(kept + 1, 0));
        assert!(page.facts.has_more, "a dropped span leaves an unshown line");
        assert!(
            page.facts.window_truncated,
            "an overrun page reports its truncation"
        );
        // The kept prefix is intact and nothing beyond it leaked in.
        assert!(page.body.contains(&format!("{:>6} | ", 1)));
        assert!(!page.body.contains(&format!("{:>6} | ", kept + 2)));
    }
}

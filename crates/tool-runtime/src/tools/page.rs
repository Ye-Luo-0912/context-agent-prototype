//! Shared final-budget paging machinery for the read tools (G2/G3, tenth
//! batch).
//!
//! Every tool that pages source content to the model renders its page
//! under the FINAL model-content budget — the same number the trusted
//! output broker clamps `model_content` to and the runtime's last-line
//! guard re-checks. A page that fits here reaches the model verbatim, so
//! the broker's head+tail preview never has to cut a page's middle, and
//! the continuation cursor is derived from what the FINAL body actually
//! contains — never from an internal capture position the broker might
//! have trimmed away.
//!
//! This module is the ONE production maintenance entry for the rule
//! "the continuation never advances past an undelivered source position":
//! [`finalize_within_budget`] measures the final envelope and drops
//! trailing spans until it fits; callers then derive cursors and coverage
//! claims from the kept spans only. Tool-specific pieces (clause texts,
//! header formats, capture loops) stay with each tool.

use agent_contracts::MAX_TOOL_MODEL_CONTENT_CHARS;

use super::with_coverage_footer;

/// The FINAL model-content budget one page is rendered under. The spec
/// declares the same constant (`ToolSpec::output_budget`) so broker and
/// capture loops enforce one number from one definition.
pub(crate) const FINAL_BODY_CHARS: usize = MAX_TOOL_MODEL_CONTENT_CHARS;

/// Rendered width of the `{:>6} | ` numbering prefix for a line number —
/// 6-wide for 1-6 digit numbers, wider beyond (a fixed reservation would
/// under-count 7+ digit line numbers).
pub(crate) fn rendered_prefix_chars(line: usize) -> usize {
    line.to_string().len().max(6) + 3
}

/// One delivered source interval: a contiguous shown span of one source
/// line. `line` is the TRUE source line number — never a collection
/// index — so rendering cannot renumber (G3).
#[derive(Debug, Clone)]
pub(crate) struct DeliveredSpan {
    pub(crate) line: usize,
    /// Position inside the line just past the shown span, in the source's
    /// own coordinate system (raw bytes for artifact reads; for whole-line
    /// reads the full content length).
    pub(crate) shown_to: usize,
    /// Every content byte of the line is shown (a cut EOL is structure,
    /// not an unshown region).
    pub(crate) complete: bool,
    /// Rendered text of the shown span, without the numbering prefix and
    /// without a trailing newline.
    pub(crate) text: String,
}

/// Render the numbered body lines from the delivered spans, separated by
/// newlines (source line numbers, never collection order).
pub(crate) fn render_numbered_spans(spans: &[DeliveredSpan]) -> String {
    let mut body = String::new();
    for span in spans {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("{:>6} | {}", span.line, span.text));
    }
    body
}

/// The finalized page.
pub(crate) struct FinalizedPage {
    pub(crate) body: String,
    /// The delivered spans kept in the final body (possibly fewer than
    /// captured, after an overrun fallback). Callers derive cursors and
    /// coverage claims from THESE spans only.
    pub(crate) spans: Vec<DeliveredSpan>,
}

/// THE single production maintenance entry for the "continuation respects
/// the delivered position" rule (G2/G3). Renders the final model body via
/// `render` (tool-specific header, numbered spans, and coverage footer),
/// re-measures it against [`FINAL_BODY_CHARS`], and drops the LAST
/// delivered spans until the body fits. Because the returned spans bound
/// every cursor and claim the caller derives, a continuation can never
/// name a source position the model did not receive — and a head+tail
/// preview is never mistaken for a delivered interval.
pub(crate) fn finalize_within_budget(
    mut spans: Vec<DeliveredSpan>,
    render: impl Fn(&[DeliveredSpan]) -> (String, Option<String>),
) -> FinalizedPage {
    let body = loop {
        let (content, footer) = render(&spans);
        let body = with_coverage_footer(content, footer);
        if body.chars().count() <= FINAL_BODY_CHARS || spans.is_empty() {
            break body;
        }
        // The envelope overran the FINAL budget: the last span is not
        // provably inside the delivered body. Drop it and re-render —
        // header claims and footer fall back with it.
        spans.pop();
    };
    FinalizedPage { body, spans }
}

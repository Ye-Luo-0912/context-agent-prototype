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

use super::Tool;

/// Captured window cap: at most this many artifact bytes are returned per
/// call regardless of the line range.
const MAX_READ_BYTES: usize = 2 * 1024 * 1024;
const MAX_READ_LINES: usize = 400;
/// `{:>6} | ` numbering prefix added to every rendered line; the capture
/// budget reserves this per line so the FINAL rendered content stays
/// within [`MAX_READ_BYTES`].
const RENDER_PREFIX_CHARS: usize = 8;
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
    #[serde(default = "default_end_line")]
    end_line: usize,
}

fn default_start_line() -> usize {
    1
}
fn default_end_line() -> usize {
    200
}

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
                    "end_line": {"type": "integer", "minimum": 1}
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
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        if args.end_line - args.start_line + 1 > MAX_READ_LINES {
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
        let capture_cap = MAX_READ_BYTES - MAX_READ_LINES * RENDER_PREFIX_CHARS;
        let mut captured = String::new();
        let mut captured_bytes = 0usize;
        let mut captured_truncated = false;
        let mut line_bytes = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut counted_lines = 0usize;
        let mut last_captured_line = 0usize;
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
            if counted_lines >= args.start_line && counted_lines <= args.end_line {
                // The capture budget applies to the RENDERED text: raw
                // bytes can expand under lossy UTF-8 rendering (one
                // invalid byte becomes a three-byte replacement char), so
                // budgeting raw bytes could still overrun the cap.
                let rendered = String::from_utf8_lossy(&line_bytes);
                let room = capture_cap.saturating_sub(captured_bytes);
                if room == 0 {
                    // Capture budget exhausted: the remaining window lines
                    // exist but are not shown. `last_captured_line` stays
                    // behind so the paging cursor points at them.
                } else if rendered.len() <= room {
                    captured_bytes += rendered.len();
                    captured.push_str(&rendered);
                    last_captured_line = counted_lines;
                } else {
                    // W06 复核反例：截断必须消费预算、必须以行边界收尾，
                    // 否则截断尾与下一行拼接、预算声明失真。
                    let mut take = room;
                    while take > 0 && !rendered.is_char_boundary(take) {
                        take -= 1;
                    }
                    captured.push_str(&rendered[..take]);
                    if !captured.ends_with('\n') {
                        captured.push('\n');
                    }
                    captured_bytes += take;
                    captured_truncated = true;
                    last_captured_line = counted_lines;
                }
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
            .map(|(offset, line)| format!("{:>6} | {}", args.start_line + offset, line))
            .collect::<Vec<_>>()
            .join("\n");
        // The cursor must not hide unshown data: lines captured-then-
        // dropped from the window (capture cap) and lines beyond the scan
        // budget both leave pages unreadable only if `has_more` lied.
        let first_unshown_in_window = (last_captured_line + 1).max(args.start_line);
        let in_window_unshown = first_unshown_in_window <= args.end_line.min(counted_lines);
        let beyond_window = scan_complete && args.end_line < counted_lines;
        let has_more = !scan_complete || in_window_unshown || beyond_window;
        let next_start_line = if in_window_unshown {
            first_unshown_in_window
        } else if has_more {
            args.end_line + 1
        } else {
            args.end_line
        };

        Ok(ToolOutcome::Value(
            ToolOutput {
                call_id: call_id.into(),
                tool_name: "artifact.read".into(),
                ok: true,
                summary: format!(
                    "read lines {}-{} of {} ({} lines total{}{})",
                    args.start_line,
                    args.start_line + lines.len().saturating_sub(1),
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
                model_content: if selected.is_empty() {
                    "no lines in range".to_string()
                } else {
                    selected
                },
                artifact_ref: Some(args.reference),
                metadata: json!({
                    "total_lines": counted_lines,
                    "total_lines_complete": scan_complete,
                    "bytes": scanned_bytes,
                    "returned": lines.len(),
                    "has_more": has_more,
                    "next_start_line": next_start_line,
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

    /// W06 复核反例：一条 3 MiB 的长首行＋100 行尾部。截断必须消费捕获
    /// 预算、以行边界收尾（不得把截断尾与 tail-0 拼接成一行）、返回字符
    /// 串不得超出声明的捕获预算，且游标必须指向第一个未展示的行。
    #[tokio::test]
    async fn long_first_line_truncates_at_the_cap_without_merging_lines() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();
        let mut body = "l".repeat(3 * 1024 * 1024);
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
        assert_eq!(
            output.model_content.lines().count(),
            1,
            "the truncated long line is one rendered line"
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
        assert_eq!(
            output.metadata["next_start_line"], 2,
            "the cursor must point at the first unshown line"
        );

        // The next page is reachable and shows the tail lines untouched.
        let page2 = value(
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
        assert!(page2.model_content.contains("tail-0"));
        assert!(page2.model_content.contains("tail-99"));
        assert_eq!(page2.metadata["total_lines"], 101);
        assert_eq!(page2.metadata["window_truncated"], false);
        assert_eq!(page2.metadata["has_more"], false);
    }
}

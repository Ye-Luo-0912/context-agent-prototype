//! Shared bounded stream handling for the process-executing tools
//! (`shell.exec`, `process.run`): stdout/stderr lines land in a bounded
//! tail for the model and are appended incrementally to an artifact, so
//! arbitrarily large logs never live in memory or in the prompt.

use std::collections::VecDeque;

use agent_contracts::{AgentError, AgentResult};
use tokio::io::{AsyncWriteExt, BufWriter};

pub(crate) const MODEL_OUTPUT_CHARS: usize = 12_000;
pub(crate) const BUFFER_LINES: usize = 200;
pub(crate) const MAX_LINE_CHARS: usize = 4_000;

pub(crate) enum StreamLine {
    Stdout(String),
    Stderr(String),
}

/// Record one output line: append it to the artifact (unbounded) and keep
/// it in the bounded model-facing tail.
pub(crate) async fn record_line(
    line: &str,
    tail: &mut VecDeque<String>,
    artifact: &mut BufWriter<tokio::fs::File>,
    total_lines: &mut usize,
    total_chars: &mut usize,
) -> AgentResult<()> {
    *total_lines += 1;
    *total_chars += line.len();
    artifact
        .write_all(line.as_bytes())
        .await
        .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;
    artifact
        .write_all(b"\n")
        .await
        .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;

    let bounded_line: String = if line.chars().count() > MAX_LINE_CHARS {
        let truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
        format!("{truncated}...[line truncated]")
    } else {
        line.to_string()
    };
    if tail.len() >= BUFFER_LINES {
        tail.pop_front();
    }
    tail.push_back(bounded_line);
    Ok(())
}

/// The bounded model-facing tail of a large output.
pub(crate) fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!(
        "...[{} chars omitted; showing tail]\n{}",
        skip,
        text.chars().skip(skip).collect::<String>()
    )
}

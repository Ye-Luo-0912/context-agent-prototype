//! Trusted output broker for the composition root.
//!
//! `WorkspaceOutputBroker` bounds every model-facing field of a tool output
//! and spills oversized content to the run's artifact directory once, so
//! the model sees a bounded preview plus an `artifact://` reference — and a
//! producer that did not spill cannot lose the truncated middle. Applied by
//! the kernel before a `ToolOutcome` reaches the actor; the actor's last-line
//! guard remains as a second, cheaper defense.

use std::sync::Arc;

use agent_contracts::{
    MAX_TOOL_METADATA_BYTES, MAX_TOOL_MODEL_CONTENT_CHARS, MAX_TOOL_OUTPUT_TOTAL_CHARS,
    MAX_TOOL_SUMMARY_CHARS, OutputBroker, RunId, ToolOutput,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::Workspace;

/// Keep the head and tail of an over-limit string and insert a visible
/// marker that names the field, the original size and (when present) the
/// artifact reference, so truncation is always honest and the full content
/// stays reachable.
fn truncate_with_marker(value: &str, limit: usize, field: &str, location: Option<&str>) -> String {
    let char_count = value.chars().count();
    if char_count <= limit {
        return value.to_string();
    }
    let reference = match location {
        Some(reference) => {
            let reference: String = reference.chars().take(256).collect();
            format!(" Full output: {reference}.")
        }
        None => String::new(),
    };
    let marker = format!(
        "\n...[output broker truncated {field} from {char_count} chars to the {limit}-char cap.{reference}]...\n"
    );
    let marker_chars = marker.chars().count();
    let content_budget = limit.saturating_sub(marker_chars);
    let head_budget = content_budget / 2;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let head: String = value.chars().take(head_budget).collect();
    let mut tail: Vec<char> = value.chars().rev().take(tail_budget).collect();
    tail.reverse();
    format!("{head}{marker}{}", tail.into_iter().collect::<String>())
}

/// Bound the metadata value: keep it as-is when its serialized size fits,
/// otherwise replace it with a bounded marker object that keeps the decoded
/// total honest (the full metadata is spilled with the content when the
/// content is spilled too).
fn bound_metadata(metadata: Value) -> Value {
    let serialized = match serde_json::to_string(&metadata) {
        Ok(json) => json,
        Err(_) => return Value::String("metadata is not serializable".into()),
    };
    if serialized.len() <= MAX_TOOL_METADATA_BYTES {
        return metadata;
    }
    serde_json::json!({
        "truncated": true,
        "field": "metadata",
        "original_bytes": serialized.len(),
        "cap_bytes": MAX_TOOL_METADATA_BYTES,
    })
}

/// Composition-root `OutputBroker` backed by the workspace artifact store.
/// Spills land under `.focus-agent/artifacts/<run>/...` like every other
/// artifact, so they share the same lifetime and cleanup rules.
pub struct WorkspaceOutputBroker {
    workspace: Arc<Workspace>,
}

impl WorkspaceOutputBroker {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl OutputBroker for WorkspaceOutputBroker {
    async fn bound(&self, run_id: RunId, mut output: ToolOutput) -> ToolOutput {
        // 1. Every field gets its own cap; the marker names what was cut.
        output.summary =
            truncate_with_marker(&output.summary, MAX_TOOL_SUMMARY_CHARS, "summary", None);
        output.metadata = bound_metadata(std::mem::take(&mut output.metadata));

        // 2. Oversized content spills to an artifact once. A producer that
        //    already returned a reference keeps it; a producer that did not
        //    spill no longer loses the truncated middle — the full content
        //    is stored and the preview points at it.
        let char_count = output.model_content.chars().count();
        if char_count > MAX_TOOL_MODEL_CONTENT_CHARS {
            if output.artifact_ref.is_none() {
                match self
                    .workspace
                    .write_artifact(
                        run_id,
                        "tool-output",
                        "txt",
                        output.model_content.as_bytes(),
                    )
                    .await
                {
                    Ok(reference) => output.artifact_ref = Some(reference),
                    Err(error) => {
                        output.artifact_ref = None;
                        output.model_content = format!(
                            "{}\n...[output broker could not spill oversized tool output to an artifact: {error}]...\n",
                            truncate_with_marker(
                                &output.model_content,
                                MAX_TOOL_MODEL_CONTENT_CHARS,
                                "model_content",
                                None
                            )
                        );
                        return output;
                    }
                }
            }
            output.model_content = truncate_with_marker(
                &output.model_content,
                MAX_TOOL_MODEL_CONTENT_CHARS,
                "model_content",
                output.artifact_ref.as_deref(),
            );
        }

        // 3. Decoded-total cap: even when each field individually fits, the
        //    combined model-facing view must stay bounded. Trim content
        //    first (the artifact reference keeps the full text reachable).
        let metadata_chars = serde_json::to_string(&output.metadata)
            .map(|json| json.chars().count())
            .unwrap_or(0);
        let total_chars =
            output.summary.chars().count() + output.model_content.chars().count() + metadata_chars;
        if total_chars > MAX_TOOL_OUTPUT_TOTAL_CHARS {
            let content_allowance = MAX_TOOL_OUTPUT_TOTAL_CHARS
                .saturating_sub(output.summary.chars().count() + metadata_chars);
            output.model_content = truncate_with_marker(
                &output.model_content,
                content_allowance,
                "model_content",
                output.artifact_ref.as_deref(),
            );
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ToolOutput;
    use serde_json::json;

    async fn workspace() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        (
            Arc::new(Workspace::open(&path).await.expect("open workspace")),
            dir,
        )
    }

    fn output(
        content: String,
        summary: String,
        metadata: Value,
        artifact_ref: Option<String>,
    ) -> ToolOutput {
        ToolOutput {
            call_id: "call".into(),
            tool_name: "producer.tool".into(),
            ok: true,
            summary,
            model_content: content,
            artifact_ref,
            metadata,
        }
    }

    /// Read back the spilled artifact bytes for a run: the broker writes
    /// `tool-output-*.txt` under the run's artifact directory.
    async fn read_spilled(workspace: &Workspace, run_id: RunId) -> Vec<u8> {
        let dir = workspace
            .state_dir()
            .join("artifacts")
            .join(run_id.to_string());
        let mut entries = tokio::fs::read_dir(&dir).await.expect("artifact dir");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("tool-output-") {
                return tokio::fs::read(entry.path()).await.expect("artifact bytes");
            }
        }
        panic!("no spilled tool-output artifact under {dir:?}");
    }

    #[tokio::test]
    async fn preserves_output_within_every_cap() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let out = output(
            "好".repeat(MAX_TOOL_MODEL_CONTENT_CHARS),
            "done".into(),
            json!({"k": "v"}),
            None,
        );
        let bounded = broker.bound(RunId::new(), out.clone()).await;
        assert_eq!(bounded.model_content, out.model_content);
        assert_eq!(bounded.summary, "done");
        assert_eq!(bounded.metadata, json!({"k": "v"}));
        assert!(bounded.artifact_ref.is_none());
    }

    #[tokio::test]
    async fn oversized_content_spills_to_an_artifact_and_keeps_both_ends() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace.clone());
        let run_id = RunId::new();
        let content = format!("START{}END", "界".repeat(MAX_TOOL_MODEL_CONTENT_CHARS * 2));
        let bounded = broker
            .bound(
                run_id,
                output(content.clone(), "done".into(), Value::Null, None),
            )
            .await;
        assert_eq!(
            bounded.model_content.chars().count(),
            MAX_TOOL_MODEL_CONTENT_CHARS
        );
        assert!(bounded.model_content.starts_with("START"));
        assert!(bounded.model_content.ends_with("END"));
        assert!(bounded.model_content.contains("output broker truncated"));
        let reference = bounded
            .artifact_ref
            .expect("spill must produce a reference");
        assert!(reference.starts_with("artifact://"));
        assert!(bounded.model_content.contains(&reference));
        // The full content is stored once, not truncated away.
        let bytes = read_spilled(&workspace, run_id).await;
        assert_eq!(String::from_utf8(bytes).unwrap(), content);
    }

    #[tokio::test]
    async fn existing_reference_is_preserved_not_overwritten() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let content = "x".repeat(MAX_TOOL_MODEL_CONTENT_CHARS + 1);
        let bounded = broker
            .bound(
                RunId::new(),
                output(
                    content,
                    "done".into(),
                    Value::Null,
                    Some("artifact://producer".into()),
                ),
            )
            .await;
        assert_eq!(bounded.artifact_ref.as_deref(), Some("artifact://producer"));
        assert!(bounded.model_content.contains("artifact://producer"));
    }

    #[tokio::test]
    async fn summary_and_metadata_are_capped_independently() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let bounded = broker
            .bound(
                RunId::new(),
                output(
                    "body".into(),
                    "s".repeat(MAX_TOOL_SUMMARY_CHARS * 2),
                    serde_json::json!({"big": "v".repeat(MAX_TOOL_METADATA_BYTES * 2)}),
                    None,
                ),
            )
            .await;
        assert_eq!(bounded.summary.chars().count(), MAX_TOOL_SUMMARY_CHARS);
        assert!(
            bounded.summary.contains("output broker truncated"),
            "the summary must carry a truncation marker"
        );
        assert_eq!(bounded.metadata["truncated"], json!(true));
        assert_eq!(bounded.model_content, "body", "small content is untouched");
    }

    #[tokio::test]
    async fn decoded_total_cap_trims_content_when_fields_combine_over() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let content = "c".repeat(MAX_TOOL_MODEL_CONTENT_CHARS - 1);
        let summary = "s".repeat(MAX_TOOL_SUMMARY_CHARS);
        let bounded = broker
            .bound(RunId::new(), output(content, summary, Value::Null, None))
            .await;
        let total = bounded.summary.chars().count()
            + bounded.model_content.chars().count()
            + serde_json::to_string(&bounded.metadata)
                .unwrap()
                .chars()
                .count();
        assert!(
            total <= MAX_TOOL_OUTPUT_TOTAL_CHARS,
            "decoded total {total} must stay under the cap"
        );
    }
}

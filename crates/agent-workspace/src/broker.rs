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

use crate::{MAX_ARTIFACT_REFERENCE_BYTES, Workspace};

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

    async fn normalized_reference_for_run(&self, run_id: RunId, reference: &str) -> Option<String> {
        if reference.len() > MAX_ARTIFACT_REFERENCE_BYTES {
            return None;
        }
        let Ok((normalized, _file)) = self
            .workspace
            .open_artifact_for_run(reference, run_id)
            .await
        else {
            return None;
        };
        Some(normalized)
    }
}

#[async_trait]
impl OutputBroker for WorkspaceOutputBroker {
    async fn bound(
        &self,
        run_id: RunId,
        budget: Option<usize>,
        mut output: ToolOutput,
    ) -> ToolOutput {
        // The effective model-content cap: the tool's declared budget,
        // clamped to the global hard cap so a declaration can never make
        // the model-facing result larger than the contract allows.
        let content_cap = budget
            .unwrap_or(MAX_TOOL_MODEL_CONTENT_CHARS)
            .min(MAX_TOOL_MODEL_CONTENT_CHARS);

        // 1. Every field gets its own cap; the marker names what was cut.
        output.summary =
            truncate_with_marker(&output.summary, MAX_TOOL_SUMMARY_CHARS, "summary", None);
        output.metadata = bound_metadata(std::mem::take(&mut output.metadata));

        // 2. A producer-supplied locator is untrusted. For content that stays
        //    inline, keep only a normalized, current-run, pinned-readable
        //    regular file. A locator alone cannot prove that it contains the
        //    exact bytes this broker is about to truncate, so every truncation
        //    below receives a fresh broker-owned spill and replaces it.
        let char_count = output.model_content.chars().count();
        let mut broker_spilled = false;
        if char_count > content_cap {
            output.artifact_ref = self
                .workspace
                .write_artifact(
                    run_id,
                    "tool-output",
                    "txt",
                    output.model_content.as_bytes(),
                )
                .await
                .ok();
            broker_spilled = output.artifact_ref.is_some();
            output.model_content = truncate_with_marker(
                &output.model_content,
                content_cap,
                "model_content",
                output.artifact_ref.as_deref(),
            );
        } else if let Some(reference) = output.artifact_ref.take() {
            output.artifact_ref = self.normalized_reference_for_run(run_id, &reference).await;
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
            // This path truncates content even though its per-field cap fit.
            // Store the still-complete field under a fresh trusted reference;
            // an arbitrary producer reference is not evidence of these bytes.
            if !broker_spilled {
                output.artifact_ref = self
                    .workspace
                    .write_artifact(
                        run_id,
                        "tool-output",
                        "txt",
                        output.model_content.as_bytes(),
                    )
                    .await
                    .ok();
            }
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

    /// 读回 broker 溢出的字节：现在落在 `<run>/tool-output/<digest>`。
    async fn read_spilled(workspace: &Workspace, run_id: RunId) -> Vec<u8> {
        let dir = workspace
            .state_dir()
            .join("artifacts")
            .join(run_id.to_string())
            .join("tool-output");
        let mut entries = tokio::fs::read_dir(&dir).await.expect("artifact owner dir");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            if entry.file_type().await.expect("type").is_file() {
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
        let bounded = broker.bound(RunId::new(), None, out.clone()).await;
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
                None,
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
    async fn readable_regular_reference_is_preserved_while_content_stays_inline() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace.clone());
        let run_id = RunId::new();
        let reference = workspace
            .write_artifact(run_id, "producer", "txt", b"producer body")
            .await
            .unwrap();
        let content = "small inline result".to_string();
        let bounded = broker
            .bound(
                run_id,
                None,
                output(
                    content.clone(),
                    "done".into(),
                    Value::Null,
                    Some(reference.clone()),
                ),
            )
            .await;
        assert_eq!(bounded.artifact_ref.as_deref(), Some(reference.as_str()));
        assert_eq!(bounded.model_content, content);
    }

    #[tokio::test]
    async fn oversized_content_always_replaces_a_producer_reference() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace.clone());
        let run_id = RunId::new();
        let producer_reference = workspace
            .write_artifact(run_id, "producer", "txt", b"unrelated producer body")
            .await
            .unwrap();
        let content = "x".repeat(MAX_TOOL_MODEL_CONTENT_CHARS + 1);
        let bounded = broker
            .bound(
                run_id,
                None,
                output(
                    content.clone(),
                    "done".into(),
                    Value::Null,
                    Some(producer_reference.clone()),
                ),
            )
            .await;

        let trusted = bounded.artifact_ref.expect("broker spill reference");
        assert_ne!(trusted, producer_reference);
        assert!(bounded.model_content.contains(&trusted));
        let (_normalized, file) = workspace
            .open_artifact_for_run(&trusted, run_id)
            .await
            .unwrap();
        let bytes = tokio::fs::read(file.display()).await.unwrap();
        assert_eq!(bytes, content.as_bytes());
    }

    #[tokio::test]
    async fn directory_reference_is_not_preserved_as_an_artifact() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace.clone());
        let run_id = RunId::new();
        workspace
            .write_artifact(run_id, "seed", "txt", b"seed")
            .await
            .unwrap();
        let directory_reference = format!("artifact://.focus-agent/artifacts/{run_id}");
        let bounded = broker
            .bound(
                run_id,
                None,
                output(
                    "small".into(),
                    "done".into(),
                    Value::Null,
                    Some(directory_reference),
                ),
            )
            .await;
        assert!(bounded.artifact_ref.is_none());
    }

    #[tokio::test]
    async fn cross_run_or_forged_reference_is_replaced_before_truncation() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace.clone());
        let producer_run = RunId::new();
        let current_run = RunId::new();
        let cross_run = workspace
            .write_artifact(producer_run, "producer", "txt", b"other run")
            .await
            .unwrap();
        let content = "x".repeat(MAX_TOOL_MODEL_CONTENT_CHARS + 1);

        let bounded = broker
            .bound(
                current_run,
                None,
                output(content, "done".into(), Value::Null, Some(cross_run.clone())),
            )
            .await;

        let replacement = bounded
            .artifact_ref
            .expect("oversized content must receive a trusted spill reference");
        assert_ne!(replacement, cross_run);
        assert!(replacement.contains(&current_run.to_string()));
        assert!(bounded.model_content.contains(&replacement));
    }

    #[tokio::test]
    async fn summary_and_metadata_are_capped_independently() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let bounded = broker
            .bound(
                RunId::new(),
                None,
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
            .bound(
                RunId::new(),
                None,
                output(content, summary, Value::Null, None),
            )
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

    #[tokio::test]
    async fn declared_tool_budget_bounds_content_before_the_global_cap() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let declared: usize = 256;
        let content = format!("START{}END", "x".repeat(declared * 2));
        let bounded = broker
            .bound(
                RunId::new(),
                Some(declared),
                output(content.clone(), "done".into(), Value::Null, None),
            )
            .await;
        assert_eq!(
            bounded.model_content.chars().count(),
            declared,
            "the declared tool budget must bound content below the global cap"
        );
        assert!(bounded.model_content.starts_with("START"));
        assert!(bounded.model_content.ends_with("END"));
        assert!(
            bounded.model_content.contains("output broker truncated"),
            "the truncation marker must name the cut"
        );
        assert!(
            bounded.artifact_ref.is_some(),
            "oversized content still spills to an artifact"
        );
    }

    #[tokio::test]
    async fn declared_budget_never_exceeds_the_global_hard_cap() {
        let (workspace, _dir) = workspace().await;
        let broker = WorkspaceOutputBroker::new(workspace);
        let content = "x".repeat(MAX_TOOL_MODEL_CONTENT_CHARS * 2);
        let bounded = broker
            .bound(
                RunId::new(),
                Some(MAX_TOOL_MODEL_CONTENT_CHARS * 10),
                output(content.clone(), "done".into(), Value::Null, None),
            )
            .await;
        assert_eq!(
            bounded.model_content.chars().count(),
            MAX_TOOL_MODEL_CONTENT_CHARS,
            "a declaration can never exceed the global hard cap"
        );
    }
}

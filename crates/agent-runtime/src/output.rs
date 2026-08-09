//! Runtime-owned last-line guard for model-facing tool content.
//!
//! Individual tools should spill large results to an artifact and return a
//! bounded `model_content`. Dynamic/process capabilities are less trusted,
//! so the actor enforces the bound again before a result enters the turn
//! frame, context engine, or event stream.

use agent_contracts::ToolOutput;

/// Matches the reference context engine's per-item ceiling. Producers may
/// choose smaller limits; none may make the model-facing result larger.
pub(crate) const MAX_TOOL_MODEL_CONTENT_CHARS: usize = 16_000;

pub(crate) fn bound_tool_output(mut output: ToolOutput) -> ToolOutput {
    let char_count = output.model_content.chars().count();
    if char_count <= MAX_TOOL_MODEL_CONTENT_CHARS {
        return output;
    }

    let location = match output.artifact_ref.as_deref() {
        Some(reference) => {
            let reference: String = reference.chars().take(256).collect();
            format!(" Full output: {reference}.")
        }
        None => " The producer did not provide an artifact.".to_string(),
    };
    let marker = format!(
        "\n...[runtime truncated model-facing tool output from {char_count} chars to the {MAX_TOOL_MODEL_CONTENT_CHARS}-char hard limit.{}]...\n",
        location
    );
    let marker_chars = marker.chars().count();
    let content_budget = MAX_TOOL_MODEL_CONTENT_CHARS.saturating_sub(marker_chars);
    let head_budget = content_budget / 2;
    let tail_budget = content_budget.saturating_sub(head_budget);
    let head: String = output.model_content.chars().take(head_budget).collect();
    let mut tail: Vec<char> = output
        .model_content
        .chars()
        .rev()
        .take(tail_budget)
        .collect();
    tail.reverse();

    output.model_content = format!("{head}{marker}{}", tail.into_iter().collect::<String>());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(content: String, artifact_ref: Option<String>) -> ToolOutput {
        ToolOutput {
            call_id: "call".into(),
            tool_name: "external.tool".into(),
            ok: true,
            summary: "done".into(),
            model_content: content,
            artifact_ref,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn preserves_output_within_the_limit() {
        let content = "好".repeat(MAX_TOOL_MODEL_CONTENT_CHARS);
        let bounded = bound_tool_output(output(content.clone(), None));
        assert_eq!(bounded.model_content, content);
    }

    #[test]
    fn bounds_unicode_output_and_preserves_both_ends() {
        let content = format!("START{}END", "界".repeat(MAX_TOOL_MODEL_CONTENT_CHARS * 2));
        let bounded = bound_tool_output(output(content, Some("artifact://full".into())));
        assert_eq!(
            bounded.model_content.chars().count(),
            MAX_TOOL_MODEL_CONTENT_CHARS
        );
        assert!(bounded.model_content.starts_with("START"));
        assert!(bounded.model_content.ends_with("END"));
        assert!(bounded.model_content.contains("runtime truncated"));
        assert!(bounded.model_content.contains("artifact://full"));
    }
}

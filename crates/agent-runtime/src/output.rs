//! Runtime-owned last-line guard for model-facing tool content.
//!
//! Individual tools should spill large results to an artifact and return a
//! bounded `model_content`; the composition root's `OutputBroker` bounds and
//! spills again before a result reaches the actor. Dynamic/process
//! capabilities are less trusted, so the actor enforces the bound one more
//! time before a result enters the turn frame, context engine, or event
//! stream.

use agent_contracts::{MAX_TOOL_MODEL_CONTENT_CHARS, ToolOutput};

/// Provider/model error text reaches events and the user; cap it so a
/// hostile or buggy provider cannot flood the journal with one message.
pub(crate) const MAX_PROVIDER_ERROR_CHARS: usize = 4_000;

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

/// Bound a provider/model error message before it enters the event stream:
/// keep the head, then a marker naming the original size.
pub(crate) fn bound_error_message(message: String) -> String {
    let char_count = message.chars().count();
    if char_count <= MAX_PROVIDER_ERROR_CHARS {
        return message;
    }
    let marker = format!(
        "\n...[runtime truncated error text from {char_count} chars to the {MAX_PROVIDER_ERROR_CHARS}-char cap]...\n"
    );
    let budget = MAX_PROVIDER_ERROR_CHARS.saturating_sub(marker.chars().count());
    let head: String = message.chars().take(budget).collect();
    format!("{head}{marker}")
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

    #[test]
    fn provider_error_within_cap_is_preserved() {
        let message = "e".repeat(MAX_PROVIDER_ERROR_CHARS);
        let bounded = bound_error_message(message.clone());
        assert_eq!(bounded, message);
    }

    #[test]
    fn oversized_provider_error_is_capped_with_a_marker() {
        let message = format!("START{}END", "x".repeat(MAX_PROVIDER_ERROR_CHARS * 2));
        let bounded = bound_error_message(message);
        assert_eq!(bounded.chars().count(), MAX_PROVIDER_ERROR_CHARS);
        assert!(bounded.starts_with("START"));
        assert!(bounded.contains("runtime truncated error text"));
    }
}

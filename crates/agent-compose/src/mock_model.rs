//! A trivial model transport used by the interactive composition root when
//! no provider key is configured: echoes the latest user request and
//! exercises one canned `fs.list` tool call for the "demo: list files"
//! prompt. Replace with a real provider adapter to evaluate model quality.

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelRole, ModelTransport, ToolCall,
};
use serde_json::json;

pub struct MockModelTransport;

#[async_trait::async_trait]
impl ModelTransport for MockModelTransport {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 4096,
            context_window: None,
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let current = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == ModelRole::User)
            .map(|message| message.content.as_str())
            .unwrap_or("");
        // A tool result is present when the runtime turn frame carried one
        // (role == Tool), regardless of how the context engine rendered it.
        let has_tool_result = request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::Tool);

        if current.trim().eq_ignore_ascii_case("demo: list files") && !has_tool_result {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "mock-list-1".into(),
                    name: "fs.list".into(),
                    arguments: json!({"path": "", "limit": 80}),
                }],
                usage: Default::default(),
            });
        }

        // The write half of the demo flow: one bounded workspace write,
        // then a plain-text completion once the tool result is in the
        // turn frame. Together with "demo: list files" this exercises the
        // read -> mutate path end to end without a model vendor.
        if current.trim().eq_ignore_ascii_case("demo: write hello") {
            if !has_tool_result {
                return Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "mock-write-1".into(),
                        name: "fs.write".into(),
                        arguments: json!({
                            "path": "hello.txt",
                            "content": "hello from the demo agent",
                        }),
                    }],
                    usage: Default::default(),
                });
            }
            return Ok(ModelOutput {
                content: "[mock model] wrote hello.txt — demo write complete.".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            });
        }

        let context_blocks = request
            .messages
            .iter()
            .filter(|m| m.content.starts_with("SELECTED WORKING CONTEXT"))
            .count();
        Ok(ModelOutput {
            content: format!(
                "[mock model] current focus request: {current}\n\nworking-context blocks supplied: {context_blocks}.\nReplace MockModelTransport with a real provider adapter to evaluate model quality."
            ),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::ModelMessage;

    fn request(user: &str, with_tool_result: bool) -> ModelRequest {
        let mut messages = vec![ModelMessage {
            role: ModelRole::User,
            content: user.into(),
            name: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        if with_tool_result {
            messages.push(ModelMessage {
                role: ModelRole::Tool,
                content: "[tool result]".into(),
                name: None,
                tool_calls: Vec::new(),
                tool_call_id: Some("mock-write-1".into()),
            });
        }
        ModelRequest {
            messages,
            tools: Vec::new(),
            metadata: serde_json::json!({}),
            cancel: agent_contracts::CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn demo_write_emits_one_fs_write_then_a_plain_completion() {
        let mock = MockModelTransport;
        let first = mock
            .complete(request("demo: write hello", false))
            .await
            .unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "fs.write");
        assert_eq!(first.tool_calls[0].arguments["path"], json!("hello.txt"),);

        let second = mock
            .complete(request("demo: write hello", true))
            .await
            .unwrap();
        assert!(second.tool_calls.is_empty());
        assert!(second.content.contains("hello.txt"));
    }

    #[tokio::test]
    async fn demo_list_keeps_its_canned_listing_call() {
        let mock = MockModelTransport;
        let output = mock
            .complete(request("demo: list files", false))
            .await
            .unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].name, "fs.list");
    }
}

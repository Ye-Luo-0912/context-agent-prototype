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

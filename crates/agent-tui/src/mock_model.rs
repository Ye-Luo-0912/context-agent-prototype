use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, ToolCall,
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
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let current = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == agent_contracts::ModelRole::User)
            .map(|message| message.content.as_str())
            .unwrap_or("");
        let has_tool_observation = request
            .messages
            .iter()
            .any(|message| message.content.contains("ToolObservation"));

        if current.trim().eq_ignore_ascii_case("demo: list files") && !has_tool_observation {
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

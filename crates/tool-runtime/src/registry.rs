use std::{collections::HashMap, sync::Arc};

use agent_contracts::{
    AgentError, AgentResult, ToolDispatcher, ToolExecutionRequest, ToolOutput, ToolSpec,
};
use agent_workspace::Workspace;

use crate::tools::{
    EditReplaceTool, FsListTool, FsReadTool, FsWriteTool, GitDiffTool, GitStatusTool,
    SearchGrepTool, ShellExecTool, Tool,
};

pub struct BuiltinToolDispatcher {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl BuiltinToolDispatcher {
    pub fn new(workspace: Workspace) -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FsListTool::new(workspace.clone())),
            Arc::new(FsReadTool::new(workspace.clone())),
            Arc::new(FsWriteTool::new(workspace.clone())),
            Arc::new(SearchGrepTool::new(workspace.clone())),
            Arc::new(EditReplaceTool::new(workspace.clone())),
            Arc::new(GitStatusTool::new(workspace.clone())),
            Arc::new(GitDiffTool::new(workspace.clone())),
            Arc::new(ShellExecTool::new(workspace)),
        ];
        let tools = tools
            .into_iter()
            .map(|tool| (tool.spec().name.clone(), tool))
            .collect();
        Self { tools }
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for BuiltinToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<_> = self.tools.values().map(|tool| tool.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let tool = self
            .tools
            .get(&request.call.name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {}", request.call.name)))?;
        tool.execute(
            request.run_id,
            &request.call.id,
            request.call.arguments,
            request.cancel,
        )
        .await
    }
}

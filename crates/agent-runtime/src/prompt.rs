//! Prompt assembly: the single place the model input is rendered.
//!
//! The context engine returns a structured working set (`MaterializedContext`)
//! and nothing more. The runtime owns the system prompt, the focus frame, the
//! context frame, the turn stack and the tool schemas; this module turns
//! those five layers into the `ModelInput` sent to the provider.

use agent_contracts::{
    FocusState, MaterializedContext, ModelInput, ModelMessage, ToolSpec, TurnFrame,
};

/// Assembles the five-layer model input for one model request.
///
/// ```text
/// System Policy        - standing instructions, owned by the runtime
/// Focus Frame          - the current task/goal from the materialized context
/// Context Frame        - the selected working set, rendered from structured items
/// Turn Frame           - the current turn's execution stack
/// Active Tool Schemas  - tool definitions carried by the model request
/// ```
pub struct PromptAssembler {
    system_prompt: String,
}

impl PromptAssembler {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
        }
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// The system prompt's own token cost, so the runtime can hand the rest
    /// of the model budget to the context engine.
    pub fn system_prompt_tokens(&self) -> usize {
        agent_contracts::tokens::approx_tokens(&self.system_prompt)
    }

    pub fn assemble(
        &self,
        materialized: &MaterializedContext,
        turn: &TurnFrame,
        tools: Vec<ToolSpec>,
    ) -> ModelInput {
        let mut context_frame = Vec::new();
        if !materialized.items.is_empty() {
            let mut working = String::from(
                "SELECTED WORKING CONTEXT\nOnly use these prior items when they remain relevant to the current focus.",
            );
            for item in &materialized.items {
                working.push_str(&format!(
                    "\n[{:?} | {:?} | {:?}]\n{}\n",
                    item.kind, item.scope, item.state, item.content
                ));
            }
            context_frame.push(ModelMessage::system(working));
        }

        ModelInput {
            system_policy: vec![ModelMessage::system(self.system_prompt.clone())],
            focus_frame: materialized.focus.as_ref().map(render_focus),
            context_frame,
            turn_frame: turn.clone(),
            tool_schemas: tools,
        }
    }
}

fn render_focus(focus: &FocusState) -> String {
    format!(
        "CURRENT FOCUS\nGoal: {}\nPhase: {}\nCurrent query: {}\nActive entities: {}",
        focus.goal,
        focus.phase,
        focus.current_query,
        if focus.active_entities.is_empty() {
            "(none)".to_string()
        } else {
            focus.active_entities.join(", ")
        }
    )
}

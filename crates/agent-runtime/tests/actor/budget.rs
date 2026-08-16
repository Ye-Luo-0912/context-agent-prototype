use std::sync::Arc;

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{ModelMessage, ToolDispatcher};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    ModelBudget, RuntimeServices, approx_layer_tokens, engine_pack_window, spawn_runtime,
};

use crate::harness::*;

#[tokio::test]
async fn engine_receives_only_the_context_frame_budget() {
    let context = Arc::new(RecordingContextEngine::default());
    let config = CoreAuthorityConfig::default();
    let system_tokens = approx_tokens(&config.system_prompt);
    let tool_specs = OneToolDispatcher.specs();
    let tools_tokens = approx_layer_tokens(&tool_specs);
    let kernel = Arc::new(RuntimeServices::new(
        config,
        context.clone(),
        Arc::new(BudgetModel),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    // The turn is a single model round; the engine query is recorded before
    // the actor replies, so the budget is observable immediately.
    let turn_tokens = approx_layer_tokens(&[ModelMessage::user("hello")]);
    let pack_window = engine_pack_window(Some(30_000), 24_000);
    let expected =
        ModelBudget::compute(pack_window, 2_000, system_tokens, turn_tokens, tools_tokens)
            .context_frame_budget;

    {
        let queries = context.queries.lock().unwrap();
        assert_eq!(queries.len(), 1, "one model round -> one materialization");
        assert_eq!(
            queries[0].budget_tokens, expected,
            "the engine must receive the kernel pack cap minus output/system/turn/tools, not the larger provider send window"
        );
        assert!(
            pack_window < 30_000,
            "a 30k send window must not raise C's pack cap above the 24k kernel budget"
        );
    }

    handle.stop().await.unwrap();
}

use std::sync::Arc;

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{FocusState, ModelMessage, TaskAnchorView, ToolDispatcher};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    ModelBudget, RuntimeServices, approx_layer_tokens, engine_pack_window, focus_frame_tokens,
    provider_send_window, spawn_runtime,
};
use agent_workspace::capture_host_runtime_facts;

use crate::harness::*;

#[tokio::test]
async fn engine_receives_only_the_context_frame_budget() {
    for kernel_budget in [24_000, 30_000, 40_000] {
        assert_context_frame_budget(kernel_budget).await;
    }
}

async fn assert_context_frame_budget(kernel_budget: usize) {
    let context = Arc::new(RecordingContextEngine::default());
    let config = CoreAuthorityConfig {
        context_budget_tokens: kernel_budget,
        ..Default::default()
    };
    let max_rounds = config.max_tool_rounds;
    let system_tokens = approx_tokens(&config.system_prompt)
        + approx_tokens(&capture_host_runtime_facts().render());
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

    let tasks = handle.list_tasks().await.unwrap();
    let task = &tasks[0];
    let mut focus = FocusState::for_task(task.id, task.goal.clone());
    focus.current_query = "hello".into();
    let task_view = TaskAnchorView {
        revision: task.anchor_revision,
        original_goal: task.goal.clone(),
        ..TaskAnchorView::default()
    };
    let progress = agent_contracts::TaskProgressView {
        anchor_revision: task.anchor_revision,
        decision_budget: Some(agent_contracts::ModelDecisionBudget {
            current_round: 1,
            max_rounds,
        }),
        ..Default::default()
    };
    let focus_tokens = focus_frame_tokens(Some(&focus), Some(&task_view), Some(&progress));

    // The turn is a single model round; the engine query is recorded before
    // the actor replies, so the budget is observable immediately.
    let turn_tokens = approx_layer_tokens(&[ModelMessage::user("hello")]);
    let send_window = provider_send_window(Some(30_000), kernel_budget);
    let pack_window = engine_pack_window(Some(30_000), kernel_budget);
    // The provider-only headroom pays turn tokens first. That capacity must
    // not be charged twice, but it never raises the engine's pack cap.
    let turn_charged_to_pack = turn_tokens.saturating_sub(send_window - pack_window);
    let expected = ModelBudget::compute(
        pack_window,
        2_000,
        system_tokens,
        focus_tokens,
        turn_charged_to_pack,
        tools_tokens,
    )
    .context_frame_budget;

    {
        let queries = context.queries.lock().unwrap();
        assert_eq!(queries.len(), 1, "one model round -> one materialization");
        assert_eq!(
            queries[0].budget_tokens, expected,
            "Context must receive only the pack remainder; provider-only headroom pays turn tokens once"
        );
        assert!(
            queries[0].budget_tokens <= kernel_budget.min(send_window),
            "neither a larger provider window nor a larger kernel budget may inflate the pack cap"
        );
        if kernel_budget >= send_window {
            assert_eq!(
                turn_charged_to_pack, turn_tokens,
                "no headroom preserves the original charge"
            );
        } else {
            assert_eq!(
                turn_charged_to_pack, 0,
                "the small turn fits entirely in send-only headroom"
            );
        }
    }

    handle.stop().await.unwrap();
}

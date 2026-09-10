use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_contracts::{
    AgentResult, ContextEngine, ModelCapabilities, ModelOutput, ModelRequest, ModelRole,
    ModelTransport, PromptLayout, RuntimeEvent, RuntimeEventEnvelope,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use context_simple::{SimpleContextConfig, SimpleContextEngine};

#[derive(Default)]
struct CaptureModel(Mutex<Vec<ModelRequest>>);

#[async_trait::async_trait]
impl ModelTransport for CaptureModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: Some(32_000),
            max_output_tokens: 256,
            ..Default::default()
        }
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.0.lock().unwrap().push(request);
        Ok(ModelOutput {
            content: "segment finished; no task closure claimed".into(),
            tool_calls: vec![],
            usage: Default::default(),
        })
    }
}

async fn finished(events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::TurnCommitFailed { .. } => {
                    panic!("layout turn failed")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("turn must complete");
}

#[tokio::test]
async fn layout_switch_preserves_live_focus_changes_and_complete_directives() {
    for layout in [PromptLayout::Legacy, PromptLayout::CurrentStateLast] {
        let model = Arc::new(CaptureModel::default());
        let context = Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
        let services = RuntimeServices::new(
            CoreAuthorityConfig::default(),
            context.clone(),
            model.clone(),
            Arc::new(super::harness::TestToolDispatcher),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        )
        .with_prompt_layout(layout);
        let (handle, actor) = spawn_runtime(Arc::new(services));
        handle.start().await.unwrap();
        let mut events = handle.subscribe();
        for goal in ["goal-alpha", "goal-beta"] {
            handle.set_focus(goal.into()).await.unwrap();
            let directive = format!("{goal}: {}\n末尾约束必须保留。", "完整上下文 ".repeat(400));
            handle.user_message(directive.clone()).await.unwrap();
            finished(&mut events).await;
            let records = model.0.lock().unwrap();
            let request = records.last().unwrap();
            let (messages, metadata) = (&request.messages, &request.metadata);
            let boundary = request
                .prompt_reuse_boundary()
                .expect("final request has a valid boundary");
            assert!(
                messages[..boundary.message_count()]
                    .iter()
                    .all(|message| message.content != directive)
            );
            assert_eq!(
                metadata["prompt_layout"],
                serde_json::to_value(layout).unwrap()
            );
            assert!(
                messages
                    .iter()
                    .any(|m| m.role == ModelRole::User && m.content == directive)
            );
            let focus_index = messages
                .iter()
                .position(|m| m.role == ModelRole::System && m.content.contains("CURRENT FOCUS"))
                .expect("current focus must be visible");
            let focus = &messages[focus_index].content;
            assert!(boundary.message_count() <= focus_index);
            assert!(focus.contains(goal));
            if goal == "goal-beta" {
                assert!(!focus.contains("goal-alpha"));
            }
            match layout {
                PromptLayout::Legacy => assert!(focus_index < messages.len() - 1),
                PromptLayout::CurrentStateLast => assert_eq!(focus_index, messages.len() - 1),
            }
        }
        assert!(context.diagnostics().await.unwrap().focus_task_id.is_some());
        handle.stop().await.unwrap();
        actor.await.unwrap();
    }
}

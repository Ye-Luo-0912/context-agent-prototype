use std::sync::Arc;

use agent_contracts::{ToolSurfaceDemand, ToolSurfaceOmissionReason, ToolSurfaceRequirement};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};

use crate::harness::*;

#[tokio::test]
async fn checkpoint_restore_rebuilds_surface_from_suspended_task_requirements() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::evicting_on_gc());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    let mut surface_events = handle.subscribe();
    let mut turn_events = handle.subscribe();
    handle.start().await.unwrap();
    handle.set_focus("restore task roots".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let requirement = ToolSurfaceRequirement {
        tool_name: "optional.large".into(),
        demand: ToolSurfaceDemand::KeepReady,
        reason: "survive suspension and restore".into(),
    };
    handle
        .replace_task_tool_requirements(task_id, 0, vec![requirement])
        .await
        .unwrap();

    handle
        .user_message("first rooted round".into())
        .await
        .unwrap();
    let first = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;
    handle.suspend_task().await.unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();

    // Diverge both the task requirements and the issued surface counter.
    handle
        .replace_task_tool_requirements(task_id, 1, Vec::new())
        .await
        .unwrap();
    handle.activate_task(task_id).await.unwrap();
    handle.user_message("diverged round".into()).await.unwrap();
    let second = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;
    handle.suspend_task().await.unwrap();

    // Restoring an older checkpoint must recover requirement revision 1,
    // rebuild the surface rather than reuse a snapshot, and preserve
    // monotonic focus/surface identities from the live process.
    instance.restore(checkpoint).await.unwrap();
    assert!(
        handle
            .replace_task_tool_requirements(task_id, 2, Vec::new())
            .await
            .is_err(),
        "a writer holding the pre-restore revision must not pass CAS after restore"
    );
    handle.activate_task(task_id).await.unwrap();
    handle
        .user_message("continue restored work".into())
        .await
        .unwrap();
    let third = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    wait_for_turn_completed(&mut turn_events).await;

    assert_eq!(second.source_revisions.task_requirement_revision, Some(2));
    assert_eq!(third.source_revisions.task_requirement_revision, Some(3));
    assert!(third.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.demand == ToolSurfaceDemand::KeepReady
            && row.reason == ToolSurfaceOmissionReason::KeepReady
    }));
    assert!(first.surface_revision < second.surface_revision);
    assert!(second.surface_revision < third.surface_revision);
    assert!(
        second.source_revisions.focus_revision < third.source_revisions.focus_revision,
        "restoring an older checkpoint must not move the runtime focus epoch backwards"
    );
    assert_eq!(tools.load_calls(), 2);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_restore_cas_high_water_survives_a_checkpoint_that_removes_the_task() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model,
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    let handle = instance.handle();
    handle.start().await.unwrap();
    let empty_checkpoint = instance.checkpoint().await.unwrap();

    handle
        .set_focus("task that will disappear".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: String::new(),
            }],
        )
        .await
        .unwrap();
    handle.suspend_task().await.unwrap();
    let task_checkpoint = instance.checkpoint().await.unwrap();
    handle
        .replace_task_tool_requirements(task_id, 1, Vec::new())
        .await
        .unwrap();

    instance.restore(empty_checkpoint).await.unwrap();
    assert!(handle.list_tasks().await.unwrap().is_empty());
    instance.restore(task_checkpoint).await.unwrap();
    let restored = handle.list_tasks().await.unwrap();
    assert_eq!(restored[0].tool_requirement_revision, 3);
    assert_eq!(restored[0].tool_requirement_count, 1);
    assert!(
        handle
            .replace_task_tool_requirements(task_id, 2, Vec::new())
            .await
            .is_err(),
        "a task disappearing from an intermediate restore must not erase its CAS high-water mark"
    );
    instance.shutdown().await.unwrap();
}

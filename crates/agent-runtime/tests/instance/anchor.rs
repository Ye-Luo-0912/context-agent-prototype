use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{ContextEngine, ContextQuery, RuntimeEvent};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices, TaskAnchor};

use crate::harness::*;

#[tokio::test]
async fn task_anchor_update_publishes_a_bounded_event() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let anchor = TaskAnchor {
        original_goal: "refactor auth".into(),
        current_interpretation: "split the auth module".into(),
        acceptance_criteria: vec!["tests pass".into()],
        open_loops: vec!["verify edge cases".into()],
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_id, 0, anchor)
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // The bounded audit event names the task, the resulting revision, the
    // fields that moved and the authority split — never the full anchor
    // content.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged {
                task_id: event_task,
                revision: event_rev,
                changed_fields,
                patch_kind,
            } = envelope.event
            {
                saw = Some((event_task, event_rev, changed_fields, patch_kind));
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let (event_task, event_rev, changed_fields, patch_kind) =
        saw.expect("anchor update must publish its event");
    assert_eq!(event_task, task_id);
    assert_eq!(event_rev, 1);
    assert_eq!(
        patch_kind,
        agent_contracts::AnchorPatchKind::Autonomous,
        "a whole-anchor replacement that moves only evolution fields is labeled autonomous"
    );
    for field in [
        "current_interpretation",
        "acceptance_criteria",
        "open_loops",
    ] {
        assert!(
            changed_fields.iter().any(|name| name == field),
            "the event must name the moved field {field}: {changed_fields:?}"
        );
    }
    assert!(
        !changed_fields.iter().any(|name| name == "original_goal"),
        "an unchanged field must not be named: {changed_fields:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn autonomous_anchor_patch_applies_without_approval() {
    // read-only approval: autonomous patches never consult the gate, so
    // they land even under the strictest policy.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let revision = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                plan_progress: Some(vec!["read the module".into()]),
                open_loops: Some(vec!["verify edge cases".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged { patch_kind, .. } = envelope.event {
                saw = Some(patch_kind);
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(
        saw,
        Some(agent_contracts::AnchorPatchKind::Autonomous),
        "the audit event must label the autonomous patch"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_anchor_patch_clears_approval_and_is_labeled_boundary() {
    // permissive approval: the boundary patch (constraints) passes the gate.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let revision = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                constraints: Some(vec!["no dependency changes".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged { patch_kind, .. } = envelope.event {
                saw = Some(patch_kind);
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(
        saw,
        Some(agent_contracts::AnchorPatchKind::Boundary),
        "the audit event must label the boundary patch"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_anchor_patch_denied_leaves_the_anchor_untouched() {
    // read-only approval: a boundary patch (goal) is denied and must not
    // reach the task table — no revision bump, no change event.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let denied = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                original_goal: Some("rewrite from scratch".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("denied by approval policy"),
        "a denied boundary patch must error with the policy message: {denied}"
    );

    // No change event was published and the anchor is untouched.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut saw_change = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TaskAnchorChanged { .. }) {
                saw_change = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !saw_change,
        "a denied patch must not publish a change event"
    );
    assert_eq!(
        instance.handle().list_tasks().await.unwrap()[0].anchor_revision,
        0,
        "a denied patch must not bump the anchor revision"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn suspend_and_resume_preserves_anchor_without_replaying_transcript() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    let task_a = instance.handle().list_tasks().await.unwrap()[0].id;
    let anchor = TaskAnchor {
        original_goal: "task A: refactor auth".into(),
        acceptance_criteria: vec!["tests pass".into()],
        open_loops: vec!["verify edge cases".into()],
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_a, 0, anchor.clone())
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // Suspend A, work on an unrelated task, run an unrelated GC pass.
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();
    context.gc().await.unwrap();

    // Resume A: the anchor is task authority held by the runtime, not by
    // the transcript — criteria/open loops survive suspension and an
    // unrelated GC without any replay.
    instance.handle().activate_task(task_a).await.unwrap();
    let resumed = instance
        .handle()
        .list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == task_a)
        .expect("task A exists");
    assert_eq!(resumed.anchor_revision, 1);
    // An equivalent replacement is idempotent at the restored revision:
    // nothing was lost or rewritten while suspended.
    let equivalent = instance
        .handle()
        .update_task_anchor(task_a, 1, anchor)
        .await
        .unwrap();
    assert_eq!(
        equivalent, 1,
        "an equivalent anchor must not bump revision after resume"
    );
    instance.shutdown().await.unwrap();
}

/// The runtime assigns a task id on focus; the context engine must be
/// focused on the *same* task — runtime and context share one task
/// identity, never a parallel one.
#[tokio::test]
async fn runtime_task_id_matches_the_context_task_id() {
    let (instance, context) = simple_instance().await;
    let mut events = instance.handle().subscribe();

    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    let mut runtime_task_id = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && runtime_task_id.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusChanged { task_id, .. } = envelope.event {
                runtime_task_id = Some(task_id);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let runtime_task_id = runtime_task_id.expect("FocusChanged must carry the task id");

    // Engine-internal FocusState keeps the runtime task id for scoring/GC.
    // Materialize must not return it for prompt rendering.
    let snapshot = context
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        snapshot.focus.is_none() && snapshot.task.is_none(),
        "materialize is historical working context, not CURRENT FOCUS"
    );
    let checkpoint = context.checkpoint().await.unwrap();
    let engine_task = checkpoint
        .get("focus")
        .and_then(|value| value.get("task_id"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    assert_eq!(
        engine_task.as_deref(),
        Some(runtime_task_id.to_string().as_str()),
        "the context engine must be focused on the runtime's task id, not a parallel one"
    );
    instance.shutdown().await.unwrap();
}

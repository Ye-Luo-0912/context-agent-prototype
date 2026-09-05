use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{CapabilityActivation, RuntimeEvent, ToolLifecycle};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    AnchorPatch, ContextRootClaim, ModuleHost, RootClaimRole, RootClaimStrength, RuntimeInstance,
    RuntimeServices, TaskAnchor,
};

use crate::harness::*;

#[tokio::test]
async fn runtime_checkpoint_roundtrips_tasks_context_and_capabilities() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    let mut events = instance.handle().subscribe();

    // Two tasks, one with a real turn, so the task table and the context
    // engine both carry state worth restoring.
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .user_message("task A: refactor auth".into())
        .await
        .unwrap();
    // Wait for the turn to finish (checkpoint requires an idle runtime).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let completed = events
            .try_recv()
            .is_ok_and(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted));
        if completed || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();

    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(
        checkpoint.tasks.tasks.len(),
        2,
        "the checkpoint must carry the task table, not just the context engine"
    );
    assert!(checkpoint.current_task_id.is_some());
    assert!(
        checkpoint.context != serde_json::Value::Null,
        "the context payload must be present"
    );

    // The file roundtrip: serialize to JSON, parse it back.
    let bytes = serde_json::to_vec(&checkpoint).unwrap();
    let decoded: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.tasks.tasks.len(), 2);
    assert_eq!(decoded.version, agent_runtime::RUNTIME_CHECKPOINT_VERSION);
    assert!(
        decoded.authority.is_some(),
        "a durable composition must capture its Core authority marker"
    );

    // Restore into a fresh runtime: tasks come back, and the engine carries
    // the restored items and scopes. Shut down the old owner first so the
    // authority journal's exclusive writer lock transfers cleanly.
    instance.shutdown().await.unwrap();
    let (fresh, fresh_context) = durable_simple_instance(&journal_path).await;
    fresh.restore(decoded).await.unwrap();
    let tasks = fresh.handle().list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 2, "restore must bring the task table back");
    let active = tasks
        .iter()
        .find(|task| task.id == checkpoint.current_task_id.unwrap());
    assert_eq!(
        active.map(|task| task.status),
        Some(agent_runtime::TaskStatus::Active),
        "the restored active task must stay active"
    );
    let items = fresh.handle().inspect_context(usize::MAX).await.unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.kind == agent_contracts::ContextKind::UserMessage),
        "the restored engine must carry the user message items"
    );
    // Task id alignment survives the round-trip: the restored engine's
    // focus must point at the same task the runtime restored as current,
    // so runtime and context cannot drift into a split-brain after
    // recovery. Production engines leave MaterializedContext.focus empty.
    assert_eq!(
        fresh_context.focused_task_id().await,
        checkpoint.current_task_id,
        "restore must align the context focus with the runtime's current task"
    );
    fresh.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_checkpoint_cannot_restore_across_core_instances() {
    let (source, _context) = simple_instance().await;
    let checkpoint = source.checkpoint().await.unwrap();
    assert!(checkpoint.authority.is_none());

    let (fresh, _context) = simple_instance().await;
    let before = fresh.handle().list_tasks().await.unwrap();
    let error = fresh.restore(checkpoint).await.unwrap_err();
    assert!(
        error.to_string().contains("ephemeral checkpoint")
            && error.to_string().contains("cannot restore"),
        "cross-Core ephemeral restore must fail before mutation: {error}"
    );
    assert_eq!(fresh.handle().list_tasks().await.unwrap(), before);

    fresh.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn tampered_authority_marker_is_rejected_before_runtime_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    instance
        .handle()
        .set_focus("live task must survive rejected restore".into())
        .await
        .unwrap();
    let before = instance.checkpoint().await.unwrap();
    let mut tampered = before.clone();
    tampered.tasks.tasks[0].goal = "must never become visible".into();
    tampered
        .authority
        .as_mut()
        .expect("durable checkpoint carries authority")
        .state_digest = agent_contracts::AuthorityStateDigest::sha256_bytes(b"tampered");

    let error = instance.restore(tampered).await.unwrap_err();
    assert!(
        error.to_string().contains("authority checkpoint marker"),
        "authority mismatch must be explicit: {error}"
    );
    let after = instance.checkpoint().await.unwrap();
    assert_eq!(after.tasks.tasks[0].goal, before.tasks.tasks[0].goal);
    assert_eq!(after.focus_revision, before.focus_revision);
    assert_eq!(after.last_surface_revision, before.last_surface_revision);
    assert_eq!(
        after.authority, before.authority,
        "validation must not bump Core"
    );

    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_checkpoint_marker_remains_valid_after_later_epoch_transitions() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    instance
        .handle()
        .set_focus("checkpoint ancestor".into())
        .await
        .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    let checkpoint_marker = checkpoint.authority.clone().unwrap();

    instance.handle().suspend_task().await.unwrap();
    let advanced = instance.checkpoint().await.unwrap().authority.unwrap();
    assert!(advanced.last_seq > checkpoint_marker.last_seq);
    assert!(advanced.authority_epoch > checkpoint_marker.authority_epoch);

    instance.restore(checkpoint).await.unwrap();
    let restored = instance.checkpoint().await.unwrap();
    assert_eq!(restored.current_task_id, restored.tasks.active);
    assert_eq!(restored.tasks.tasks[0].goal, "checkpoint ancestor");
    assert!(
        restored.authority.unwrap().last_seq > advanced.last_seq,
        "restore advances live authority; it never rewinds to the checkpoint cursor"
    );

    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_restore_keeps_the_existing_task_and_context_authority() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();

    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.tasks[0].goal = "replacement task".into();
    // The task half is internally valid, but the opaque context payload is
    // not. Previously the actor installed the replacement task table before
    // discovering this restore error.
    invalid.context = serde_json::Value::Null;

    let result = instance.restore(invalid).await;
    assert!(
        result.is_err(),
        "the invalid context payload must be rejected"
    );
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].goal, "original task");

    assert_eq!(
        context.focused_task_id().await,
        Some(tasks[0].id),
        "the original context focus must survive failed restore"
    );
    assert_eq!(
        context.focused_goal().await.as_deref(),
        Some("original task")
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_inconsistent_redundant_task_authority() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.active = None;

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("task authority is inconsistent"),
        "the redundant authority mismatch must be diagnosed before mutation: {error}"
    );
    assert_eq!(instance.handle().list_tasks().await.unwrap(), before);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_context_focus_that_disagrees_with_task_authority() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();

    // Keep all actor-owned redundant fields internally consistent while
    // making them disagree with the opaque context checkpoint's focus.
    let replacement = agent_contracts::TaskId::new();
    invalid.tasks.tasks[0].id = replacement;
    invalid.tasks.active = Some(replacement);
    invalid.current_task_id = Some(replacement);

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("context focus"),
        "context/task disagreement must be rejected explicitly: {error}"
    );
    assert_eq!(instance.handle().list_tasks().await.unwrap(), before);
    assert_eq!(context.focused_task_id().await, Some(before[0].id));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_actor_restore_does_not_change_capability_flags() {
    let mut host = ModuleHost::new();
    host.register_capability(Arc::new(CheckpointCapability::new()))
        .unwrap();
    let registry = host.capability_registry();
    registry.enable("checkpoint-capability").await.unwrap();
    registry.load_tool("checkpoint.tool").unwrap();
    host.start().await.unwrap();

    let instance = RuntimeInstance::spawn(host, services());
    instance.start().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.version += 1;
    invalid.capabilities[0].activation = CapabilityActivation::Disabled;
    invalid.capabilities[0].loaded = false;

    assert!(instance.restore(invalid).await.is_err());
    assert_eq!(
        registry.activation("checkpoint-capability"),
        Some(CapabilityActivation::Enabled),
        "a rejected actor restore must not partially apply capability activation"
    );
    assert_eq!(
        registry.tool_state("checkpoint.tool"),
        Some(ToolLifecycle::Loaded),
        "a rejected actor restore must not unload the existing surface"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_event_reports_only_capabilities_registered_in_the_live_host() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    let mut checkpoint = instance.checkpoint().await.unwrap();
    checkpoint
        .capabilities
        .push(agent_runtime::CapabilitySnapshot {
            id: "missing-from-live-host".into(),
            activation: CapabilityActivation::Enabled,
            loaded: true,
            loaded_tools: vec!["missing.tool".into()],
        });

    instance.restore(checkpoint).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let applied = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "restore event was not published");
        let envelope = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("restore event timeout")
            .expect("restore event channel closed");
        if let RuntimeEvent::RuntimeRestored {
            capabilities_applied,
            ..
        } = envelope.event
        {
            break capabilities_applied;
        }
    };
    assert!(
        !applied,
        "an unknown-only checkpoint capability set must not be reported as applied"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_full_restores_are_serialized_across_all_state_planes() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let journal = Arc::new(BlockingFirstRestoreJournal::new(entered_tx, release_rx));
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = Arc::new(RuntimeInstance::spawn(
        host,
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContextEngine),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(journal.clone()),
        ),
    ));
    instance.start().await.unwrap();
    let checkpoint_a = agent_runtime::RuntimeCheckpoint {
        version: agent_runtime::RUNTIME_CHECKPOINT_VERSION,
        unresolved_ack_debts: Vec::new(),
        run_metadata: agent_runtime::RunMetadata {
            run_id: instance.handle().run_id(),
            created_at_ms: 0,
            provider_profile_digest: String::new(),
        },
        tasks: agent_runtime::TaskManagerSnapshot {
            tasks: Vec::new(),
            active: None,
            completed: Vec::new(),
        },
        current_task_id: None,
        focus_revision: 0,
        last_surface_revision: 0,
        context: serde_json::Value::Null,
        capabilities: Vec::new(),
        authority: None,
        snapshot_sequence: 3,
        capability_generation: 4,
        event_cover_seq: 0,
        terminal_commit: false,
    };
    let mut checkpoint_b = checkpoint_a.clone();
    checkpoint_b.focus_revision = 41;

    let first_instance = instance.clone();
    let first = tokio::spawn(async move { first_instance.restore(checkpoint_a).await });
    tokio::time::timeout(Duration::from_secs(2), entered_rx)
        .await
        .expect("first restore did not reach its durable finalize barrier")
        .expect("first restore dropped the barrier signal");

    let second_instance = instance.clone();
    let second = tokio::spawn(async move { second_instance.restore(checkpoint_b).await });

    // If the full transaction were not gated, B could prepare while A is
    // blocked at finalize, replacing A's pending token and eventually
    // allowing a late capability-plane write after B unfenced the actor.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "the second full restore crossed the first restore transaction"
    );

    release_tx.send(()).expect("release first restore barrier");
    tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first restore did not finish")
        .expect("first restore task panicked")
        .expect("first restore failed");
    tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("second restore did not finish")
        .expect("second restore task panicked")
        .expect("second restore failed");

    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.focus_revision > 41,
        "the second restore must commit after the first, not interleave with it"
    );
    let instance = Arc::try_unwrap(instance).unwrap_or_else(|_| panic!("test leaked instance"));
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_emits_the_bounded_restore_commit_event() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();
    let task_a = instance.handle().list_tasks().await.unwrap()[0].id;
    let checkpoint = instance.checkpoint().await.unwrap();

    // Push task A's tool-requirement revision past the checkpoint's value,
    // so restoring the older checkpoint must rebase it (CAS-ABA fence).
    instance
        .handle()
        .replace_task_tool_requirements(task_a, 0, Vec::new())
        .await
        .unwrap();

    instance.restore(checkpoint.clone()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut restored = None;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::RuntimeRestored { .. } = envelope.event {
                restored = Some(envelope.event);
                break;
            }
        }
        if restored.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let RuntimeEvent::RuntimeRestored {
        checkpoint_version,
        restored_run_id,
        current_run_id,
        focus_revision,
        surface_revision,
        rebased_tasks,
        rebased_task_sample,
        capabilities_applied,
    } = restored.expect("restore must publish its bounded restore-commit event")
    else {
        panic!("restore event is not the expected variant");
    };

    assert_eq!(
        checkpoint_version,
        agent_runtime::RUNTIME_CHECKPOINT_VERSION,
        "the event names the restored checkpoint version"
    );
    assert_eq!(
        restored_run_id, current_run_id,
        "an in-process round-trip restores the same run"
    );
    assert_eq!(
        restored_run_id, checkpoint.run_metadata.run_id,
        "the event names the run that produced the checkpoint"
    );
    assert!(
        focus_revision.effective > focus_revision.old,
        "restore bumps the focus revision into a fresh epoch: {focus_revision:?}"
    );
    assert!(
        surface_revision.effective >= surface_revision.restored
            && surface_revision.effective >= surface_revision.old,
        "the surface revision never moves backwards: {surface_revision:?}"
    );
    // Both tasks carry a tool-requirement revision at or below the live
    // high-water mark (task A was advanced, task B ties), so both are
    // rebased forward.
    assert_eq!(
        rebased_tasks, 2,
        "the event counts every rebased task requirement revision"
    );
    assert_eq!(
        rebased_task_sample.len(),
        2,
        "the capped sample carries the rebased task ids"
    );
    assert!(
        !capabilities_applied,
        "an empty capability surface records nothing applied"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_audit_failure_demands_recovery_and_fences_mutation() {
    let (source, _context) = simple_instance().await;
    source
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let mut checkpoint = source.checkpoint().await.unwrap();

    // The actor with a journal that refuses the restore-commit record.
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let failing = RuntimeInstance::spawn(
        host,
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            // The real reference engine: restore must pass the
            // context/task focus agreement check and reach the journal
            // barrier before the audit failure can surface.
            Arc::new(context_simple::SimpleContextEngine::new(
                context_simple::SimpleContextConfig::default(),
            )),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(Arc::new(FailRestoreEventJournal)),
        ),
    );
    failing.start().await.unwrap();
    // This test isolates the final event-journal barrier. With no durable
    // operation journal, an ephemeral checkpoint is valid only inside the
    // same live run, so align its provenance with this deliberately bare
    // test Core before exercising the later audit failure.
    checkpoint.run_metadata.run_id = failing.handle().run_id();
    let mut events = failing.handle().subscribe();

    let error = failing.restore(checkpoint).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("simulated restore-commit journal failure"),
        "the audit failure must surface from restore: {error}"
    );

    // The standard recovery signal is emitted when possible.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_recovery = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::RecoveryRequired) {
                saw_recovery = true;
            }
        }
        if saw_recovery {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_recovery, "the runtime must emit the recovery signal");

    // Normal mutation is rejected until a known-good restore lands.
    let fenced = failing
        .handle()
        .set_focus("another task".into())
        .await
        .unwrap_err();
    assert!(
        fenced.to_string().contains("recovery is required"),
        "mutation must be fenced after a restore whose audit event failed: {fenced}"
    );
    failing.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_anchor_survives_checkpoint_restore() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let anchor = TaskAnchor {
        original_goal: "refactor auth".into(),
        current_interpretation: "split the auth module".into(),
        constraints: vec!["no dependency changes".into()],
        acceptance_criteria: vec!["tests pass".into()],
        plan_progress: vec!["read the module".into()],
        open_loops: vec!["verify edge cases".into()],
        working_refs: vec![ContextRootClaim {
            item_ref: "item:auth".into(),
            role: RootClaimRole::ActiveDecision,
            strength: RootClaimStrength::ResidentRequired,
            source_field_id: "plan_progress".into(),
        }],
        evidence_refs: Vec::new(),
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_id, 0, anchor)
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // The checkpoint carries the full anchor (authority, not a scored item).
    let checkpoint = instance.checkpoint().await.unwrap();
    let snapshot = &checkpoint.tasks.tasks[0];
    assert_eq!(snapshot.anchor.revision, 1);
    assert_eq!(
        snapshot.anchor.current_interpretation,
        "split the auth module"
    );
    assert_eq!(
        snapshot.anchor.working_refs[0].role,
        RootClaimRole::ActiveDecision
    );

    // Restoring the checkpoint brings the anchor back, revision intact
    // (anchor revisions are task authority, never rebased like surface
    // revisions).
    instance.restore(checkpoint).await.unwrap();
    let info = &instance.handle().list_tasks().await.unwrap()[0];
    assert_eq!(info.id, task_id);
    assert_eq!(info.anchor_revision, 1);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_completed_task_without_a_completion_record() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap();

    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.completed.clear();

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("no committed completion record"),
        "a completed task must own exactly one outcome: {error}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_plan_view_survives_a_checkpoint_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;

    instance
        .handle()
        .set_focus("migrate the config module".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    // The view exists before any plan is written and reads back empty.
    let view = instance.handle().task_plan_view().await.unwrap().unwrap();
    assert_eq!(view.original_goal, "migrate the config module");
    assert!(
        view.plan_progress.is_empty() && view.open_loops.is_empty() && view.next_action.is_empty()
    );

    // The same autonomous CAS lane `task.manage` uses; a stale base
    // revision is refused with the existing feedback and loses nothing.
    let revision = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            AnchorPatch {
                plan_progress: Some(vec!["[x] locate the reader".into()]),
                next_action: Some("add the target key".into()),
                ..AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);
    let refused = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            AnchorPatch {
                plan_progress: Some(vec!["stale write".into()]),
                ..AnchorPatch::default()
            },
        )
        .await;
    assert!(refused.is_err(), "a stale base revision must be refused");

    let view = instance.handle().task_plan_view().await.unwrap().unwrap();
    assert_eq!(
        view.plan_progress,
        vec!["[x] locate the reader".to_string()]
    );
    assert_eq!(view.next_action, "add the target key");

    // Save and restore into a fresh runtime: the checklist, the next
    // action and the TaskId all come back.
    let checkpoint = instance.checkpoint().await.unwrap();
    instance.shutdown().await.unwrap();
    let (fresh, _context) = durable_simple_instance(&journal_path).await;
    fresh.restore(checkpoint).await.unwrap();
    let tasks = fresh.handle().list_tasks().await.unwrap();
    let restored = tasks.iter().find(|task| task.id == task_id);
    assert_eq!(
        restored.map(|task| task.status),
        Some(agent_runtime::TaskStatus::Active),
        "the checklist's task must survive with its identity"
    );
    let view = fresh.handle().task_plan_view().await.unwrap().unwrap();
    assert_eq!(
        view.plan_progress,
        vec!["[x] locate the reader".to_string()]
    );
    assert_eq!(view.next_action, "add the target key");
    assert_eq!(view.revision, 1);
    fresh.shutdown().await.unwrap();
}

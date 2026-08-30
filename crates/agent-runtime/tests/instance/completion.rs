use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    ContextEngine, ContextIngress, ContextMaintenanceTrigger, ContextSearchQuery, FocusState,
    RuntimeEvent, TaskId, ToolOutput,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices, TaskAnchor};

use crate::harness::*;

/// Full-suite acceptance for `context.admit`: a model tool call travels the
/// whole route — scripted model emits `context.manage`, the dispatcher
/// returns the typed runtime directive, the actor executes it at
/// operation-commit time, and the engine re-enters the externalized item
/// under its original id. The result is the admission event itself, not a
/// duplicated observation.
#[tokio::test]
async fn context_manage_admit_routes_end_to_end() {
    // 种入一个被外部化的条目（buffer 溢出触发 full GC 写入 store）。
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            gc_buffer_capacity: 1,
            gc_reactivate_per_pass: 8,
            context_store_dir: Some(dir.path().to_path_buf()),
            ..context_simple::SimpleContextConfig::default()
        },
    ));
    context
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "service layer"),
        })
        .await
        .unwrap();
    context
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for (index, ch) in ["x", "y"].iter().enumerate() {
        context
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: format!("step-{index}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("step {index}: fix AuthService.rs {}", ch.repeat(160)),
                    artifact_ref: None,
                    metadata: serde_json::json!({"path": "AuthService.rs"}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    context
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc_report = context.gc().await.unwrap();
    assert!(gc_report.externalized >= 1, "the seed must externalize");

    let refs = context
        .search_external(ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            label: None,
            limit: 16,
        })
        .await
        .unwrap();
    let target = refs[0].item_id;
    let resident_before = context.diagnostics().await.unwrap().resident_items;

    // 构造实例：真实 context 引擎 + 返回 admit 指令的 dispatcher +
    // 先发工具调用后完成的 scripted model。
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(AdmitScriptedModel::new(target)),
        Arc::new(AdmitDirectiveDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("service layer".into())
        .await
        .unwrap();
    instance
        .handle()
        .user_message("admit the step I need".into())
        .await
        .unwrap();

    // 等待 turn 完成（admit 指令在操作提交时执行）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let completed = events
            .try_recv()
            .is_ok_and(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted));
        if completed || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        context.diagnostics().await.unwrap().resident_items > resident_before,
        "the admitted item must be resident after the turn"
    );
    let inspected = context.inspect(100).await.unwrap();
    assert!(
        inspected.iter().any(|item| item.id == target),
        "the admitted item is inspectable under its original id"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_commits_a_typed_record_and_publishes_task_identity() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    // Advance the anchor so the completion record names a non-trivial
    // revision: the outcome is measured against exactly that authority.
    instance
        .handle()
        .update_task_anchor(
            task_id,
            0,
            TaskAnchor {
                original_goal: "refactor auth".into(),
                acceptance_criteria: vec!["tests pass".into()],
                ..TaskAnchor::default()
            },
        )
        .await
        .unwrap();

    instance
        .handle()
        .complete_current_task("auth refactor shipped".into())
        .await
        .unwrap();

    // The typed event carries the task/result identity, not free text only.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskCompleted {
                task_id: event_task,
                anchor_revision,
                summary,
            } = envelope.event
            {
                saw = Some((event_task, anchor_revision, summary));
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let (event_task, anchor_revision, summary) =
        saw.expect("completion must publish its typed event");
    assert_eq!(event_task, task_id);
    assert_eq!(anchor_revision, 1);
    assert_eq!(summary, "auth refactor shipped");

    // The checkpoint carries the immutable record; restore brings it back.
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    assert_eq!(checkpoint.tasks.completed[0].task_id, task_id);
    assert_eq!(checkpoint.tasks.completed[0].anchor_revision, 1);
    assert_eq!(
        checkpoint.tasks.completed[0].summary,
        "auth refactor shipped"
    );
    assert_eq!(
        checkpoint.tasks.completed[0].disposition,
        agent_runtime::task::CompletionDisposition::OperatorOverride,
        "an explicit operator completion must remain distinguishable from verified readiness"
    );
    assert!(
        checkpoint.tasks.completed[0]
            .unmet_reasons
            .iter()
            .any(|reason| matches!(
                reason,
                agent_runtime::task::CompletionBlocker::OperatorClosureOnly
            )),
        "the durable override must retain the task's actual completion-policy blocker"
    );
    assert_eq!(
        checkpoint.tasks.tasks[0].status,
        agent_runtime::TaskStatus::Completed
    );

    instance.restore(checkpoint).await.unwrap();
    assert_eq!(
        instance.handle().list_tasks().await.unwrap()[0].status,
        agent_runtime::TaskStatus::Completed,
        "the completed task stays completed after restore"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_completion_schedules_storage_gc_and_publishes_the_report() {
    // Task completion is the explicit runtime boundary for Storage GC: the
    // completed task's records stop being storage roots at this point, so
    // one conservative Storage GC pass runs right after the completion GC —
    // never on the per-model hot path — and its report is published as a
    // `StorageGc` event, the only permanent-deletion surface.
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::StorageGc { report } = envelope.event {
                saw = Some(report);
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let report = saw.expect("task completion must schedule one storage GC pass");
    // The empty engine has nothing to delete, but the pass still ran and
    // reported; io_errors stay 0 — an IO failure is never mistaken for
    // "the file is already gone".
    assert_eq!(report.io_errors, 0);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_failure_never_leaves_a_half_closed_task() {
    // The context side refuses the completion ingest: the transaction must
    // fail before the task authority plane commits, so the task stays
    // Active, the active slot stays, and no outcome record exists.
    let instance = RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(FailingCompleteEngine),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ),
    );
    instance.start().await.unwrap();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();

    let error = instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("simulated completion ingest failure"),
        "the completion failure must surface: {error}"
    );

    // No half-closed task: still Active, active slot intact, no record.
    let after = instance.handle().list_tasks().await.unwrap();
    assert_eq!(after, before, "a failed completion changes nothing");
    assert_eq!(after[0].status, agent_runtime::TaskStatus::Active);
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.tasks.completed.is_empty(),
        "no outcome was committed"
    );
    assert_eq!(checkpoint.current_task_id, before[0].id.into());
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_audit_gap_marks_recovery_but_keeps_the_commit() {
    // The completion itself commits (context + task authority together), but
    // the mandatory typed event cannot be journaled: the runtime must keep
    // the aligned committed state, mark recovery-required and emit the
    // standard recovery signal — never report an un-audited success.
    let instance = RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(context_simple::SimpleContextEngine::new(
                context_simple::SimpleContextConfig::default(),
            )),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(Arc::new(FailCompletionEventJournal)),
        ),
    );
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();

    let error = instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("audit event failed"),
        "the audit gap must surface explicitly: {error}"
    );

    // The standard recovery signal is emitted.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_recovery = false;
    while tokio::time::Instant::now() < deadline && !saw_recovery {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::RecoveryRequired) {
                saw_recovery = true;
            }
        }
        if !saw_recovery {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(saw_recovery, "the runtime must emit the recovery signal");

    // The aligned state stayed committed: the task is Completed and the
    // runtime fences normal mutation until a known-good restore. (Checkpoint
    // itself is refused while recovery is required — that refusal is part of
    // the fence, and restore is the one mutation that may clear it.)
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, agent_runtime::TaskStatus::Completed);
    let fenced = instance
        .handle()
        .set_focus("another task".into())
        .await
        .unwrap_err();
    assert!(
        fenced.to_string().contains("recovery is required"),
        "mutation must be fenced after an audit gap: {fenced}"
    );
    let fenced_checkpoint = instance.checkpoint().await.unwrap_err();
    assert!(
        fenced_checkpoint
            .to_string()
            .contains("recovery is required"),
        "checkpoint must also be fenced while recovery is required"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn thousand_completed_tasks_stay_bounded_and_searchable() {
    let (instance, context) = simple_instance().await;
    for index in 0..1000 {
        instance
            .handle()
            .set_focus(format!("task {index}: fix component {index}"))
            .await
            .unwrap();
        instance
            .handle()
            .complete_current_task(format!("component {index} fixed"))
            .await
            .unwrap();
    }

    // Every completed task owns exactly one committed outcome: the runtime's
    // task catalog holds all of them, and the checkpoint persists each one.
    assert_eq!(instance.handle().list_tasks().await.unwrap().len(), 1000);
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1000);
    assert!(
        checkpoint
            .tasks
            .tasks
            .iter()
            .all(|task| task.status == agent_runtime::TaskStatus::Completed)
    );

    // The context working set stays bounded: completing 1,000 tasks must not
    // grow the resident heap linearly with the task count. A completed
    // task's records are storage roots, not residency roots.
    let diagnostics = context.diagnostics().await.unwrap();
    assert!(
        diagnostics.resident_items < 200,
        "resident heap must stay bounded after 1,000 completed tasks, got {}",
        diagnostics.resident_items
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_record_carries_a_verifiable_final_output_digest() {
    use sha2::{Digest, Sha256};

    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;
    let summary = "auth refactor shipped";
    instance
        .handle()
        .complete_current_task(summary.into())
        .await
        .unwrap();

    // The completion record names the exact final-output body and its
    // digest, so the outcome is byte-for-byte verifiable.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = &checkpoint.tasks.completed[0];
    let mut hasher = Sha256::new();
    hasher.update(summary.as_bytes());
    let expected = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        record.final_output_digest.as_deref(),
        Some(expected.as_str()),
        "the record carries the digest of the exact final output"
    );
    assert_eq!(
        record.final_output_ref.as_deref(),
        Some(format!("task:{task_id}:completion").as_str()),
        "the record carries a deterministic ref to its own final output"
    );

    // Restart (restore) keeps the outcome and its digest intact.
    instance.restore(checkpoint).await.unwrap();
    let restored = instance.checkpoint().await.unwrap();
    let restored_record = &restored.tasks.completed[0];
    assert_eq!(restored_record.summary, summary);
    assert_eq!(
        restored_record.final_output_digest.as_deref(),
        Some(expected.as_str()),
        "the digest survives a restore unchanged"
    );
    instance.shutdown().await.unwrap();
}

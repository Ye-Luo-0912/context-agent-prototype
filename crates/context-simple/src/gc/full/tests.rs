use super::*;
use crate::engine::SimpleContextEngine;
use crate::index::entity::extract_entities;
use agent_contracts::{
    ContextAction, ContextEngine, ContextHints, ContextIngress, ContextItemId, ContextKind,
    ContextMaintenanceTrigger, ContextQuery, ContextResidency, ContextRetention, FocusState,
    TaskId, ToolOutput,
};
use serde_json::json;

#[tokio::test]
async fn gc_evicts_consumed_ephemeral_observations_with_a_reason() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    // A successful observation is ephemeral and leaves attention after
    // the turn (consumed, not tombstoned — it stays recallable).
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "tests passed in AuthService.rs".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let before = engine.diagnostics().await.unwrap();
    assert!(before.archived_items >= 1, "ephemeral observation consumed");

    let report = engine.gc().await.unwrap();
    assert!(
        report.evicted >= 1,
        "gc must evict the consumed observation"
    );
    assert!(
        report
            .evictions
            .iter()
            .any(|e| e.reason.contains("observation consumed")),
        "eviction must be explainable, got: {:?}",
        report.evictions
    );

    let after = engine.diagnostics().await.unwrap();
    assert_eq!(
        after.warm_items, 1,
        "the consumed observation leaves the heap for the reversible buffer"
    );
    assert_eq!(
        after.total_items, 2,
        "the logical catalog keeps the user message in the heap and the consumed observation in the warm buffer"
    );
}

async fn file_read_turn(engine: &SimpleContextEngine, path: &str, body: &str) {
    engine
        .ingest(ContextIngress::UserMessage {
            content: format!("refactor {path}"),
        })
        .await
        .unwrap();
    let model_content = body
        .lines()
        .enumerate()
        .map(|(index, line)| format!("{:>6} | {line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: format!("read of {path}"),
                model_content,
                artifact_ref: None,
                metadata: json!({
                    "path": path,
                    "revision": "test-rev",
                }),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
}

fn snapshot_text(snapshot: &agent_contracts::MaterializedContext) -> String {
    snapshot
        .items
        .iter()
        .map(|item| item.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn gc_keeps_latest_file_body_when_the_task_cycles_to_another_file() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "refactor auth to async"),
        })
        .await
        .unwrap();

    file_read_turn(&engine, "src/auth/login.rs", "fn handle_21()").await;
    file_read_turn(&engine, "src/auth/session.rs", "fn handle_22()").await;

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue session.rs".into(),
            budget_tokens: 8_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let rendered = snapshot_text(&snapshot);
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.file_path.as_deref() == Some("src/auth/login.rs")),
        "live-shaped reads must carry structured path, got {:?}",
        snapshot
            .items
            .iter()
            .map(|item| item.file_path.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        rendered.contains("fn handle_21()"),
        "the previous file's latest body must stay in the working set, got {rendered}"
    );
    assert!(
        rendered.contains("fn handle_22()"),
        "the current file body must stay, got {rendered}"
    );

    // 同一路径的更新读覆盖旧正文：handle_21 离开，handle_24 留下。
    file_read_turn(&engine, "src/auth/login.rs", "fn handle_24()").await;
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue login.rs".into(),
            budget_tokens: 8_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let rendered = snapshot_text(&snapshot);
    assert!(
        !rendered.contains("fn handle_21()"),
        "a superseded file body must leave the working set, got {rendered}"
    );
    assert!(
        rendered.contains("fn handle_24()"),
        "the newer file body must stay, got {rendered}"
    );
}

#[tokio::test]
async fn gc_drops_file_bodies_when_focus_moves_to_another_task() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let first = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(first, "fix pagination"),
        })
        .await
        .unwrap();
    file_read_turn(&engine, "src/api/items.rs", "fn list()").await;

    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "add csv export"),
        })
        .await
        .unwrap();
    file_read_turn(&engine, "src/api/export.rs", "fn export()").await;

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue export.rs".into(),
            budget_tokens: 8_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let rendered = snapshot_text(&snapshot);
    assert!(
        !rendered.contains("fn list()"),
        "a previous task's file body must not contaminate the new task, got {rendered}"
    );
    assert!(
        rendered.contains("fn export()"),
        "the active task's file body must stay, got {rendered}"
    );
}

#[tokio::test]
async fn gc_marks_roots_and_evicts_stale_archived_items_by_generation() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();

    // Archive a cold item from another task, outside the active focus
    // scope tree: unmarked, past the generation cap, entities not hot.
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.kind == ContextKind::UserMessage {
                item.task_id = Some(TaskId::new());
                item.scope_id = None; // no focus-scope membership
                item.content = "fix CacheStore.rs".into();
                item.entities = extract_entities(&item.content);
                item.attention = AttentionState::Archived;
                item.relevance = 0.0;
                item.gc_generation = 99; // already past the cap
            }
        }
    }

    let report = engine.gc().await.unwrap();
    assert_eq!(report.marked_roots, 0, "no roots in the test heap");
    assert_eq!(report.evicted, 1, "the cold archived item is evicted");
    assert!(
        report.evictions[0].reason.contains("GC passes"),
        "generational reason expected, got: {}",
        report.evictions[0].reason
    );
}

#[tokio::test]
async fn gc_reactivates_warm_items_whose_entities_become_hot_again() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    // Round 1: the agent reads AuthService.rs; the successful file body
    // drops after the turn and gc evicts it.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "     1 | fn handle() {}".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.evicted >= 1, "something must be evicted first");

    // Round 2: the user asks about AuthService.rs again — its entities
    // are hot, so the next gc reactivates the evicted observations.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.reactivated >= 1, "evicted items must come back");
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("hot again")),
        "reactivation must be explainable, got: {:?}",
        report.reactivations
    );

    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.gc_reactivated_total as usize, report.reactivated,
        "cumulative counter matches"
    );
}

#[tokio::test]
async fn pathless_tool_stdout_does_not_auto_reactivate() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "touched AuthService.rs".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.evicted >= 1, "something must be evicted first");

    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivations.iter().all(|row| {
            row.kind != ContextKind::ToolObservation || !row.reason.contains("hot again")
        }),
        "pathless stdout must not be a GC identity: {:?}",
        report.reactivations
    );
}

#[tokio::test]
async fn stamped_shell_path_does_not_auto_reactivate() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "touched AuthService.rs".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.evicted >= 1, "something must be evicted first");

    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivations.iter().all(|row| {
            row.kind != ContextKind::ToolObservation || !row.reason.contains("hot again")
        }),
        "stamped-path shell stdout is identity, not a hot-recall body: {:?}",
        report.reactivations
    );
}

#[tokio::test]
async fn checked_file_body_does_not_auto_reactivate() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "     1 | fn handle() {}".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs", "revision": "abc"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.evicted >= 1, "something must be evicted first");

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: vec!["AuthService.rs@abc".into()],
            },
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivations.iter().all(|row| {
            row.kind != ContextKind::ToolObservation || !row.reason.contains("hot again")
        }),
        "a Checked path must not hot-recall its fs.read body: {:?}",
        report.reactivations
    );

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles { files: vec![] },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("hot again")),
        "clearing the projection must restore file-body recall: {:?}",
        report.reactivations
    );
}

#[tokio::test]
async fn checked_files_directive_replaces_and_caps() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: vec!["src/a.rs@1".into(), "src/b.rs@2".into()],
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.checked_files.len(), 2);
        assert_eq!(state.checked_files[0], "src/a.rs@1");
    }
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: vec!["src/c.rs@3".into()],
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.checked_files, vec!["src/c.rs@3".to_string()]);
    }
    let overflow: Vec<String> = (0..40).map(|i| format!("src/f{i}.rs@{i}")).collect();
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: overflow.clone(),
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.checked_files.len(),
            agent_contracts::MAX_CHECKED_FILE_HINTS
        );
        assert_eq!(
            state.checked_files[0],
            overflow[overflow.len() - agent_contracts::MAX_CHECKED_FILE_HINTS]
        );
    }
}

#[tokio::test]
async fn skipped_warm_file_body_is_a_prompt_descriptor() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    const BODY: &str = "fn handle_secret() {}";
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: format!("     1 | {BODY}"),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs", "revision": "abc"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    assert!(engine.gc().await.unwrap().evicted >= 1);

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: vec!["AuthService.rs@abc".into()],
            },
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivations.iter().all(|row| {
            row.kind != ContextKind::ToolObservation || !row.reason.contains("hot again")
        }),
        "Checked path must stay Warm: {:?}",
        report.reactivations
    );

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "what did we change in AuthService.rs?".into(),
            budget_tokens: 8_000,
            hints: ContextHints {
                checked_files: vec!["AuthService.rs@abc".into()],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert!(
        snapshot
            .items
            .iter()
            .all(|item| !item.content.contains(BODY)),
        "the skipped body must not re-enter SELECTED WORKING CONTEXT"
    );
    let descriptor = snapshot
        .external
        .iter()
        .find(|entry| entry.file_path.as_deref() == Some("AuthService.rs"))
        .expect("a Warm checked file body must be a reachable descriptor");
    assert_eq!(descriptor.residency, ContextResidency::Warm);
    assert_eq!(descriptor.context_ref.summary, "AuthService.rs@abc");
    assert!(
        !descriptor.context_ref.summary.contains(BODY),
        "refs only: identity, not the file text"
    );
    let fetched = engine
        .fetch_external(descriptor.item_id)
        .await
        .unwrap()
        .expect("Fetch must still return the Warm catalog body");
    assert!(
        fetched.content.contains(BODY),
        "exact body stays behind Fetch"
    );
}

#[tokio::test]
async fn gc_buffer_overflow_externalizes_instead_of_purging() {
    let store = tempfile::tempdir().unwrap();
    let config = SimpleContextConfig {
        gc_buffer_capacity: 2,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    };
    let engine = SimpleContextEngine::new(config);

    // Three turns on distinct files: each successful observation is
    // consumed, evicted, and stays evicted (its entities are not hot
    // again).
    let mut last = None;
    for i in 0..3 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("task round {i} in File{i}.rs"),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: i.to_string(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("touched File{i}.rs round {i}"),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        last = Some(engine.gc().await.unwrap());
    }

    let last = last.expect("three gc passes ran");
    assert_eq!(
        last.externalized, 1,
        "overflow externalizes the oldest eviction instead of purging"
    );
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.warm_items, 2, "buffer stays bounded");
    assert_eq!(diagnostics.cold_items, 1, "the externalized item is Cold");
    // The store keeps the full content: nothing was deleted.
    let files = std::fs::read_dir(store.path()).unwrap().count();
    assert_eq!(files, 1, "the externalized item's content lives on disk");
}

#[tokio::test]
async fn gc_generation_increments_for_survivors_and_evicts_at_the_cap() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    // A cold, unmarked working item from another task: Cooling, entities
    // not hot, so nothing marks it and the generational counter climbs.
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: false,
                summary: "fail".into(),
                model_content: "error in CacheStore.rs:42".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.kind == ContextKind::Error {
                // Cold and unmarked: another task, outside the active
                // focus scope tree, entities outside the hot set (the
                // error itself contributed CacheStore.rs to the hot set
                // at ingest, so the content must change).
                item.task_id = Some(TaskId::new());
                item.scope_id = None;
                item.content = "error in TempStore.rs:7".into();
                item.entities = extract_entities(&item.content);
                item.attention = AttentionState::Cooling;
            }
        }
    }

    let mut evicted_at_cap = None;
    for pass in 0..4 {
        let report = engine.gc().await.unwrap();
        if report
            .evictions
            .iter()
            .any(|e| e.reason.contains("GC passes"))
        {
            evicted_at_cap = Some((pass, report.evictions[0].generation));
        }
    }
    let (pass, generation) = evicted_at_cap.expect("the cooling item is evicted at the cap");
    assert_eq!(generation, 3, "eviction happens once generation 3 >= max 3");
    assert_eq!(pass, 3, "it takes the full generational ladder to evict");
}

#[tokio::test]
async fn gc_protects_dependencies_of_roots_forward_along_the_edges() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    // A: an old decision; B: the current finding that depends on A.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs as the auth layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "touched AuthService.rs".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    let (a_id, b_id) = {
        let state = engine.state.lock().await;
        let a = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("decision item");
        let b = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("finding item");
        assert!(
            b.dependencies.iter().any(|edge| edge.target == a.id),
            "the finding must depend on the decision it builds on"
        );
        (a.id, b.id)
    };
    {
        let mut state = engine.state.lock().await;
        for item in state.items.iter_mut() {
            if item.id == a_id {
                // Cold and unmarked: another task, outside the focus
                // scope tree, entities outside the hot set, past the
                // generation cap — only a residency-required edge from the
                // root can protect it now.
                item.task_id = Some(TaskId::new());
                item.scope_id = None;
                item.content = "use OldStore.rs instead".into();
                item.entities = extract_entities(&item.content);
                item.attention = AttentionState::Archived;
                item.relevance = 0.0;
                item.gc_generation = 99;
            }
            if item.id == b_id {
                item.retention = ContextRetention::Pinned;
                item.dependencies = vec![agent_contracts::DependencyEdge::continuation(a_id)];
            }
        }
    }

    let report = engine.gc().await.unwrap();
    assert!(
        report.marked_roots >= 2,
        "the root and its dependency must be marked, got {:?}",
        report.evictions
    );
    assert!(
        !report.evictions.iter().any(|e| e.item_id == a_id),
        "the dependency of a root must be protected by the forward edge, got: {:?}",
        report.evictions
    );
    let diagnostics = engine.diagnostics().await.unwrap();
    assert!(
        state_has(&engine, a_id).await,
        "the old decision must still be resident; diagnostics: {diagnostics:?}"
    );
}

#[tokio::test]
async fn gc_does_not_treat_shares_entities_as_a_residency_root() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs as the auth layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "touched AuthService.rs".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    let (a_id, b_id) = {
        let state = engine.state.lock().await;
        let a = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("decision item");
        let b = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("finding item");
        assert!(
            b.dependencies
                .iter()
                .any(|edge| edge.target == a.id && !edge.kind.requires_residency()),
            "ingest mints SharesEntities, which is not a residency root"
        );
        (a.id, b.id)
    };
    {
        let mut state = engine.state.lock().await;
        for item in state.items.iter_mut() {
            if item.id == a_id {
                item.task_id = Some(TaskId::new());
                item.scope_id = None;
                item.content = "use OldStore.rs instead".into();
                item.entities = extract_entities(&item.content);
                item.attention = AttentionState::Archived;
                item.relevance = 0.0;
                item.gc_generation = 99;
            }
            if item.id == b_id {
                item.retention = ContextRetention::Pinned;
            }
        }
    }

    let report = engine.gc().await.unwrap();
    assert!(
        report.evictions.iter().any(|e| e.item_id == a_id) || !state_has(&engine, a_id).await,
        "weak affinity must not keep the overlap target resident: evictions={:?}",
        report.evictions
    );
}

#[tokio::test]
async fn gc_never_resurrects_superseded_items() {
    // Dependency expansion is off so the (correctly protected) old
    // decision is not rooted through the new decision's dependency edge;
    // this test isolates the reactivation exclusion.
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    // A decision, then a newer decision supersedes it.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs as the auth layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs instead".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let old_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.content.contains("as the auth layer"))
            .expect("the superseded decision")
            .id
    };
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.id == old_id {
                assert!(
                    matches!(
                        item.semantic,
                        agent_contracts::SemanticState::Superseded { .. }
                    ),
                    "the older decision must be superseded, got {:?}",
                    item.semantic
                );
                // Evictable: past the generation cap, another task,
                // outside the focus scope tree, and its entities leave
                // the hot set so nothing roots it.
                item.task_id = Some(TaskId::new());
                item.scope_id = None;
                item.content = "use OldStore.rs instead".into();
                item.entities = extract_entities(&item.content);
                item.gc_generation = 99;
            }
        }
    }
    let first = engine.gc().await.unwrap();
    assert!(
        first.evictions.iter().any(|e| e.item_id == old_id),
        "the superseded item must be evicted first"
    );

    // Its old entities become hot again — but semantic death is
    // terminal: the item must stay in the reversible buffer.
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.id == old_id {
                item.entities = extract_entities("AuthService.rs");
            }
        }
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what about AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let second = engine.gc().await.unwrap();
    assert!(
        !second.reactivations.iter().any(|r| r.item_id == old_id),
        "a superseded item must never resurrect, got: {:?}",
        second.reactivations
    );
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.warm_items, 1,
        "the superseded item stays in the buffer: {diagnostics:?}"
    );
}

/// Whether the engine still holds the item in its resident heap.
async fn state_has(engine: &SimpleContextEngine, id: ContextItemId) -> bool {
    engine
        .state
        .lock()
        .await
        .items
        .iter()
        .any(|item| item.id == id)
}

#[test]
fn dependency_edges_resolve_across_heap_buffer_and_store() {
    use crate::store::to_external_entry;
    use agent_contracts::{ContextRef, ContextResidency, ContextScope, DependencyKind};

    let mut state = State::default();
    let heap_id = ContextItemId::new();
    let buffer_id = ContextItemId::new();
    let external_id = ContextItemId::new();
    let edge = |target: ContextItemId| DependencyEdge {
        target,
        kind: DependencyKind::EvidenceFor,
    };

    let mut heap_item = crate::item::make_item(
        &state,
        &SimpleContextConfig::default(),
        "heap body".into(),
        ContextKind::Decision,
        ContextScope::Task,
        ContextRetention::Working,
        0.8,
        None,
    );
    heap_item.id = heap_id;
    heap_item.dependencies.push(edge(buffer_id));
    state.items.replace_all(vec![heap_item]);

    let mut buffer_item = crate::item::make_item(
        &state,
        &SimpleContextConfig::default(),
        "buffer body".into(),
        ContextKind::ToolObservation,
        ContextScope::Task,
        ContextRetention::Working,
        0.2,
        None,
    );
    buffer_item.id = buffer_id;
    buffer_item.dependencies.push(edge(external_id));
    buffer_item.residency = ContextResidency::Warm;
    state.eviction_buffer.push(buffer_item);

    let mut external_item = crate::item::make_item(
        &state,
        &SimpleContextConfig::default(),
        "store body".into(),
        ContextKind::ToolObservation,
        ContextScope::Task,
        ContextRetention::Working,
        0.2,
        None,
    );
    external_item.id = external_id;
    external_item.dependencies.push(edge(ContextItemId::new()));
    let entry = to_external_entry(
        &external_item,
        ContextRef {
            uri: format!("context://run/{external_id}"),
            item_id: external_id,
            kind: ContextKind::ToolObservation,
            scope: ContextScope::Task,
            summary: "store body".into(),
            created_tick: 0,
        },
        0,
        0,
        None,
    );
    state.external.push(entry);

    // The traversal must find edges in all three locations: the heap,
    // the warm buffer and the external map.
    assert!(dependency_edges(&state, heap_id).is_some_and(|e| e[0].target == buffer_id));
    assert!(
        dependency_edges(&state, buffer_id).is_some_and(|e| e[0].target == external_id),
        "a demoted dependency's own edges must still be traversable"
    );
    assert!(
        dependency_edges(&state, external_id).is_some(),
        "an external entry's captured edges must be traversable"
    );
    assert!(dependency_edges(&state, ContextItemId::new()).is_none());
}

#[tokio::test]
async fn hot_reactivation_requires_exact_entity_identity() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        tool_hot_ttl_turns: 1,
        // Isolate the hot-identity gate from the score fallback.
        active_threshold: 10.0,
        ..SimpleContextConfig::default()
    });
    engine
        .ingest(ContextIngress::UserMessage {
            content: "touch src/auth/AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "     1 | fn run() {}".into(),
                artifact_ref: None,
                metadata: json!({"path": "src/auth/AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let _ = engine.gc().await.unwrap();

    // Expire the path-shaped tool-hot from the observation so the next
    // user message only carries the basename cousin.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "unrelated pause".into(),
        })
        .await
        .unwrap();

    engine
        .ingest(ContextIngress::UserMessage {
            content: "what about AuthService.rs?".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report
            .reactivations
            .iter()
            .all(|r| !r.reason.contains("hot again")),
        "substring cousins must not auto-reactivate: {:?}",
        report.reactivations
    );

    engine
        .ingest(ContextIngress::WorkingSetSignal {
            resources: vec![agent_contracts::ResourceTouch {
                path: "src/auth/AuthService.rs".into(),
                revision: None,
            }],
            content: String::new(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("hot again")),
        "exact path identity must auto-reactivate: {:?}",
        report.reactivations
    );
}

#[tokio::test]
async fn descriptor_only_tool_observation_skips_body_reactivation() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        descriptor_only_tool_observation_reactivation: true,
        ..SimpleContextConfig::default()
    });
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "     1 | fn handle() {}".into(),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let _ = engine.gc().await.unwrap();

    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::WorkingSetSignal {
            resources: vec![agent_contracts::ResourceTouch {
                path: "AuthService.rs".into(),
                revision: None,
            }],
            content: String::new(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report
            .reactivations
            .iter()
            .all(|r| r.kind != ContextKind::ToolObservation || !r.reason.contains("hot again")),
        "ablation must leave fs.read bodies Warm/Stored: {:?}",
        report.reactivations
    );
}

use agent_contracts::{
    ContextEngine, ContextHints, ContextIngress, ContextKind, ContextQuery, ContextRetention,
    ContextScope, MAX_FOREGROUND_RESOURCES, MAX_FOREGROUND_TOKENS, ResourceKey, ToolOutput, tokens,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

fn fs_read(call_id: &str, path: &str) -> ToolOutput {
    ToolOutput {
        call_id: call_id.into(),
        tool_name: "fs.read".into(),
        ok: true,
        summary: "read".into(),
        model_content: "     1 | fn body() {}".into(),
        artifact_ref: None,
        metadata: serde_json::json!({"path": path}),
    }
}

#[tokio::test]
async fn warm_body_is_projected_without_residency_change() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect src/scratch.md".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read("1", "src/scratch.md"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let Some(pos) = items
            .iter()
            .position(|item| item.file_path.as_deref() == Some("src/scratch.md"))
        else {
            panic!("the read must be resident");
        };
        let item = items.remove(pos);
        state.items.replace_all(items);
        state.eviction_buffer.push(item);
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                checked_files: vec!["src/scratch.md@rev".into()],
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), 1);
    assert!(
        materialized.foreground[0].content.contains("fn body"),
        "foreground must carry the warm body, not path@rev: {}",
        materialized.foreground[0].content
    );
    let state = engine.state.lock().await;
    assert!(
        state
            .eviction_buffer
            .iter()
            .any(|item| item.file_path.as_deref() == Some("src/scratch.md")),
        "Warm stays Warm; projection is not reactivation"
    );
    assert!(
        state
            .items
            .iter()
            .all(|item| item.file_path.as_deref() != Some("src/scratch.md")),
        "must not Admit or promote into the resident heap"
    );
}

#[tokio::test]
async fn unmentioned_known_path_is_not_foreground() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read("1", "src/util.py"),
            scope_id: None,
        })
        .await
        .unwrap();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert!(
        materialized.foreground.is_empty(),
        "engine trusts hints; util.py was not named"
    );
}

#[tokio::test]
async fn foreground_caps_at_two_resources() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    for (id, path) in [("1", "src/a.md"), ("2", "src/b.md"), ("3", "src/c.md")] {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: fs_read(id, path),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "see a.md b.md c.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                checked_files: vec![
                    "src/a.md@r".into(),
                    "src/b.md@r".into(),
                    "src/c.md@r".into(),
                ],
                foreground_resources: ["src/a.md", "src/b.md", "src/c.md"]
                    .into_iter()
                    .map(|path| ResourceKey {
                        path: path.into(),
                        revision: None,
                    })
                    .collect(),
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), MAX_FOREGROUND_RESOURCES);
    assert_eq!(
        materialized.foreground[0].file_path.as_deref(),
        Some("src/a.md")
    );
    assert_eq!(
        materialized.foreground[1].file_path.as_deref(),
        Some("src/b.md")
    );
}

#[tokio::test]
async fn foreground_clips_to_token_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    {
        let mut state = engine.state.lock().await;
        let mut body = crate::item::make_item(
            &state,
            &engine.config,
            "x".repeat(20_000),
            ContextKind::ToolObservation,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            Some("tool:fs.read".into()),
        );
        body.scope_id = None;
        body.file_path = Some("src/scratch.md".into());
        state.items.push(body);
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                checked_files: vec!["src/scratch.md@r".into()],
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), 1);
    assert!(
        tokens::approx_tokens(&materialized.foreground[0].content) <= MAX_FOREGROUND_TOKENS,
        "foreground total must stay within the request cap"
    );
}

#[tokio::test]
async fn stored_body_is_projected_without_admit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "append notes").await;
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read("1", "src/scratch.md"),
            scope_id: None,
        })
        .await
        .unwrap();
    let external_before;
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let Some(pos) = items
            .iter()
            .position(|item| item.file_path.as_deref() == Some("src/scratch.md"))
        else {
            panic!("the read must be resident");
        };
        let item = items.remove(pos);
        state.items.replace_all(items);
        let context_ref = crate::store::externalize(dir.path(), &item).unwrap();
        let entry = crate::store::to_external_entry(&item, context_ref, 0, 0, None);
        state.external.push(entry);
        external_before = state.external.iter().count();
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), 1);
    assert!(
        materialized.foreground[0].content.contains("fn body"),
        "{}",
        materialized.foreground[0].content
    );
    let state = engine.state.lock().await;
    assert_eq!(
        state.external.iter().count(),
        external_before,
        "Stored is not Admitted"
    );
    assert!(
        state
            .items
            .iter()
            .all(|item| item.file_path.as_deref() != Some("src/scratch.md"))
    );
}

#[tokio::test]
async fn foreground_deducts_actual_tokens_from_the_historical_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read("1", "src/scratch.md"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        let mut note = crate::item::make_item(
            &state,
            &engine.config,
            "historical ".repeat(20),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.9,
            Some("note:history".into()),
        );
        note.scope_id = None;
        state.items.push(note);
    }
    let fg_tokens = tokens::approx_tokens("     1 | fn body() {}");
    let historical_tokens = tokens::approx_tokens("historical ".repeat(20).as_str());
    assert!(
        historical_tokens > 10,
        "the historical note must be large enough to miss a leftover after foreground"
    );
    let budget = fg_tokens + 8;
    assert!(
        historical_tokens > budget.saturating_sub(fg_tokens),
        "historical must not fit after actual foreground is charged"
    );
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: budget,
            hints: ContextHints {
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), 1);
    assert!(
        materialized.foreground[0].content.contains("fn body"),
        "foreground packs first: {}",
        materialized.foreground[0].content
    );
    assert!(
        materialized
            .items
            .iter()
            .all(|item| !item.content.contains("historical historical")),
        "historical working set must see leftover after actual foreground, not a 2K reserve; selected={:?}",
        materialized
            .items
            .iter()
            .map(|item| item.content.chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
    );
}

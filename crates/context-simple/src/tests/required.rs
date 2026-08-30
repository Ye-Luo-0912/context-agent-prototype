use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextEngine, ContextItem, ContextItemId, ContextKind,
    ContextMaterializationMissReason, ContextQuery, ContextRetention, ContextScope, RootReason,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

fn query_for(item_id: ContextItemId, budget_tokens: usize) -> ContextQuery {
    ContextQuery {
        current_input: "continue".into(),
        budget_tokens,
        hints: agent_contracts::ContextHints {
            anchor_roots: vec![AnchorRootClaim {
                item_ref: item_id.to_string(),
                strength: AnchorRootStrength::PromptRequired,
                source_field_id: "evidence_refs".into(),
                anchor_revision: 7,
                reason: RootReason::CompletionEvidence,
            }],
            ..Default::default()
        },
    }
}

fn required_item(
    state: &crate::engine::State,
    config: &SimpleContextConfig,
    content: &str,
) -> ContextItem {
    crate::item::make_item(
        state,
        config,
        content.into(),
        ContextKind::Note,
        ContextScope::Session,
        ContextRetention::Working,
        0.1,
        Some("test:required".into()),
    )
}

async fn install_external(
    engine: &SimpleContextEngine,
    item: &ContextItem,
    checksum: Option<String>,
) {
    let context_ref = crate::store::make_context_ref(item);
    let entry = crate::store::to_external_entry(item, context_ref, 1, 1, checksum);
    let mut state = engine.state.lock().await;
    state.external.push(entry);
}

#[tokio::test]
async fn prompt_required_body_reports_budget_exclusion() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let item_id = {
        let mut state = engine.state.lock().await;
        let item = required_item(&state, &engine.config, "required evidence body");
        let id = item.id;
        state.items.push(item);
        id
    };

    let materialized = engine.materialize(query_for(item_id, 0)).await.unwrap();
    assert!(
        materialized
            .items
            .iter()
            .all(|item| item.item_id != item_id)
    );
    assert_eq!(materialized.required_misses.total(), 1);
    assert_eq!(
        materialized.required_misses.as_slice()[0].reason,
        ContextMaterializationMissReason::BudgetExcluded
    );
}

#[tokio::test]
async fn oversized_later_required_keeps_the_existing_visible_set() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let (small_required_id, oversized_required_id, optional_id) = {
        let mut state = engine.state.lock().await;
        let mut small_required = required_item(&state, &engine.config, "small pinned constraint");
        small_required.retention = ContextRetention::Pinned;
        let small_required_id = small_required.id;

        let mut oversized_required = required_item(
            &state,
            &engine.config,
            &format!("oversized pinned constraint {}", "x".repeat(8_000)),
        );
        oversized_required.retention = ContextRetention::Pinned;
        let oversized_required_id = oversized_required.id;

        let mut optional = required_item(&state, &engine.config, "useful optional observation");
        optional.importance = 1.0;
        let optional_id = optional.id;

        state.items.push(small_required);
        state.items.push(oversized_required);
        state.items.push(optional);
        (small_required_id, oversized_required_id, optional_id)
    };

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 300,
            hints: Default::default(),
        })
        .await
        .unwrap();

    assert!(materialized.required_item_ids.contains(&small_required_id));
    assert!(materialized.required_misses.as_slice().iter().any(|miss| {
        miss.identity.item_id == Some(oversized_required_id)
            && miss.reason == ContextMaterializationMissReason::BudgetExcluded
    }));
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.item_id == optional_id),
        "a required body that cannot fit after all eligible evictions must not mutate the visible set"
    );
}

#[tokio::test]
async fn absent_required_store_blob_is_typed_missing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    });
    let item = {
        let state = engine.state.lock().await;
        required_item(&state, &engine.config, "missing evidence")
    };
    let item_id = item.id;
    install_external(&engine, &item, None).await;

    let materialized = engine.materialize(query_for(item_id, 4_096)).await.unwrap();
    assert_eq!(
        materialized.required_misses.as_slice()[0].reason,
        ContextMaterializationMissReason::Missing
    );
}

#[tokio::test]
async fn checksum_mismatched_required_blob_is_typed_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    });
    let item = {
        let state = engine.state.lock().await;
        required_item(&state, &engine.config, "original evidence")
    };
    let original = serde_json::to_vec(&item).unwrap();
    let checksum = crate::store::externalize_async(dir.path(), item.id, &original)
        .await
        .unwrap();
    let mut tampered = item.clone();
    tampered.content = "different but valid JSON evidence".into();
    std::fs::write(
        dir.path().join(format!("{}.json", item.id)),
        serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();
    install_external(&engine, &item, Some(checksum)).await;

    let materialized = engine.materialize(query_for(item.id, 4_096)).await.unwrap();
    assert_eq!(
        materialized.required_misses.as_slice()[0].reason,
        ContextMaterializationMissReason::Corrupt
    );
}

#[tokio::test]
async fn required_store_operational_failure_is_typed_io_failed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    });
    let item = {
        let state = engine.state.lock().await;
        required_item(&state, &engine.config, "unreadable evidence")
    };
    std::fs::create_dir(dir.path().join(format!("{}.json", item.id))).unwrap();
    install_external(&engine, &item, None).await;

    let materialized = engine.materialize(query_for(item.id, 4_096)).await.unwrap();
    assert_eq!(
        materialized.required_misses.as_slice()[0].reason,
        ContextMaterializationMissReason::IoFailed
    );
}

#[tokio::test]
async fn cold_required_body_is_read_without_changing_residency() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    });
    let item = {
        let state = engine.state.lock().await;
        required_item(&state, &engine.config, "cold required evidence")
    };
    let bytes = serde_json::to_vec(&item).unwrap();
    let checksum = crate::store::externalize_async(dir.path(), item.id, &bytes)
        .await
        .unwrap();
    install_external(&engine, &item, Some(checksum)).await;

    let materialized = engine.materialize(query_for(item.id, 4_096)).await.unwrap();
    assert!(materialized.required_misses.is_empty());
    assert_eq!(materialized.required_item_ids, vec![item.id]);
    assert!(
        materialized
            .items
            .iter()
            .any(|selected| selected.item_id == item.id && selected.content == item.content)
    );
    let state = engine.state.lock().await;
    assert!(state.external.iter().any(|entry| entry.item_id == item.id));
    assert!(state.items.iter().all(|resident| resident.id != item.id));
}

#[tokio::test]
async fn required_miss_sample_is_bounded_and_reports_omissions() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        for index in 0..80 {
            let mut item = required_item(&state, &engine.config, &format!("pinned {index}"));
            item.retention = ContextRetention::Pinned;
            state.items.push(item);
        }
    }

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        materialized.required_misses.len(),
        agent_contracts::CONTEXT_MATERIALIZATION_MISS_CAP
    );
    assert_eq!(materialized.required_misses.omitted(), 16);
    assert_eq!(materialized.required_misses.total(), 80);
}

#[test]
fn required_planning_caps_bodies_and_store_reads_before_io() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let mut state = engine.state.blocking_lock();
    for index in 0..400 {
        let mut item = required_item(&state, &engine.config, &format!("cold pinned {index}"));
        item.retention = ContextRetention::Pinned;
        let entry = crate::store::to_external_entry(
            &item,
            crate::store::make_context_ref(&item),
            1,
            1,
            None,
        );
        state.external.push(entry);
    }

    let plan = crate::materializer::plan_required(
        &state,
        &ContextQuery {
            current_input: "continue".into(),
            budget_tokens: usize::MAX,
            hints: Default::default(),
        },
    );
    assert_eq!(
        plan.body_count(),
        agent_contracts::CONTEXT_CONSUMPTION_ACK_ITEM_CAP
    );
    assert_eq!(
        plan.store_read_count(),
        agent_contracts::CONTEXT_CONSUMPTION_ACK_ITEM_CAP,
        "planning must not enqueue more cold reads than the wire/body cap"
    );
    assert!(plan.miss_count() > 0, "overflow must remain observable");
}

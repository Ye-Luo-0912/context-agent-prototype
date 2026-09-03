use agent_contracts::{
    AccessSignal, AttentionState, CONTEXT_SEARCH_MAX_LIMIT, CONTEXT_SEARCH_MAX_QUERY_CHARS,
    ContextEngine, ContextHints, ContextIngress, ContextItemId, ContextKind,
    ContextMaintenanceTrigger, ContextQuery, ContextResidency, ContextRetention, ContextScope,
    ContextSearchQuery, CoreLabel, Label, SemanticState, TaskId, ToolOutput,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

#[tokio::test]
async fn external_retrieval_searches_inspects_and_fetches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_a = TaskId::new();
    let task_b = TaskId::new();
    let (item_a_id, item_b_id) = {
        let mut state = engine.state.lock().await;
        let mut item_a = crate::item::make_item(
            &state,
            &engine.config,
            "AuthService.rs decision: replace the auth flow".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.5,
            Some("tool-capture".into()),
        );
        item_a.id = ContextItemId::new();
        item_a.task_id = Some(task_a);
        item_a.entities = crate::index::entity::extract_entities(&item_a.content);
        item_a.tags.push(Label::core(CoreLabel::Decision));
        let mut item_b = crate::item::make_item(
            &state,
            &engine.config,
            "CacheStore.rs note: LRU eviction order".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        item_b.id = ContextItemId::new();
        item_b.task_id = Some(task_b);
        item_b.entities = crate::index::entity::extract_entities(&item_b.content);
        for (item, tick) in [(&item_a, 1u64), (&item_b, 2u64)] {
            let reference = crate::store::externalize(dir.path(), item).unwrap();
            state.external.push(crate::store::to_external_entry(
                item, reference, tick, 1, None,
            ));
        }
        (item_a.id, item_b.id)
    };

    // Search by entity signature: only the decision matches.
    let hits = engine
        .search_external(ContextSearchQuery::new("AuthService", 10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "entity search must hit exactly one ref");
    assert_eq!(hits[0].item_id, item_a_id);
    assert_eq!(hits[0].task_id, Some(task_a));

    // Kind filter without a query.
    let hits = engine
        .search_external(ContextSearchQuery {
            query: String::new(),
            kind: Some(ContextKind::Note),
            scope: None,
            task_id: None,
            label: None,
            limit: 0,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, item_b_id);

    // Task filter: task A owns exactly its own ref.
    let hits = engine
        .search_external(ContextSearchQuery {
            query: String::new(),
            kind: None,
            scope: None,
            task_id: Some(task_a),
            label: None,
            limit: 0,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, item_a_id);

    // Label is a catalog index dimension, not a residual scan predicate.
    let hits = engine
        .search_external(ContextSearchQuery {
            query: String::new(),
            label: Some("decision".into()),
            limit: 0,
            ..ContextSearchQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "label=decision must hit the tagged ref");
    assert_eq!(hits[0].item_id, item_a_id);

    {
        let state = engine.state.lock().await;
        assert_eq!(state.catalog.len(), 2, "two stored ids, one directory");
        assert_eq!(
            state.catalog.location(item_a_id),
            Some(crate::index::catalog::CatalogLocation::Stored)
        );
        assert_eq!(
            state.catalog.location(item_b_id),
            Some(crate::index::catalog::CatalogLocation::Stored)
        );
    }

    // Inspect: metadata without a store read.
    let inspected = engine
        .inspect_external(item_a_id)
        .await
        .unwrap()
        .expect("entry exists");
    assert_eq!(inspected.kind, ContextKind::Decision);
    assert!(
        inspected
            .tags
            .iter()
            .any(|t| t.is_core(CoreLabel::Decision))
    );

    // Fetch: full content back, and the access is stamped on the entry so
    // recency ranking and Cold -> External aging stay honest.
    let fetched = engine
        .fetch_external(item_a_id)
        .await
        .unwrap()
        .expect("item readable back");
    assert!(fetched.content.contains("replace the auth flow"));
    {
        let state = engine.state.lock().await;
        let entry = state
            .external
            .iter()
            .find(|e| e.item_id == item_a_id)
            .expect("entry survives the fetch");
        assert_eq!(entry.last_access_tick, state.event_seq);
        assert_eq!(entry.last_access_gc_epoch, Some(state.gc_epoch));
        assert_eq!(
            entry.last_access_signal,
            AccessSignal::Fetch,
            "fetch 读到 body，信号必须强于 search/inspect"
        );
        assert_eq!(entry.search_reinforce_count, 0);
    }

    // The item stays externalized: fetch is a read, not a reactivation. The
    // logical catalog still lists the entry, but projected from its store
    // descriptor — not as a resident body.
    let items = engine.inspect(usize::MAX).await.unwrap();
    let entry = items
        .iter()
        .find(|item| item.id == item_a_id)
        .expect("the fetched item stays part of the logical catalog");
    assert_eq!(
        entry.source.as_deref(),
        Some("tool-capture"),
        "fetch must not re-enter the working set, and the source authority survives externalization"
    );
}

#[tokio::test]
async fn catalog_search_surfaces_resident_hits_instead_of_empty() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "keep AuthService.rs in the working set".into(),
        })
        .await
        .unwrap();

    let hits = engine
        .search_external(ContextSearchQuery::new("AuthService", 8))
        .await
        .unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.residency == ContextResidency::Resident
                && hit.entities.iter().any(|e| e.contains("AuthService"))),
        "a live working-set file must be a catalog hit, not an empty miss: {hits:?}"
    );
    let resident = hits
        .iter()
        .find(|hit| hit.residency == ContextResidency::Resident)
        .unwrap();
    let inspected = engine
        .inspect_external(resident.item_id)
        .await
        .unwrap()
        .expect("resident inspect returns a descriptor");
    assert_eq!(inspected.residency, ContextResidency::Resident);
    let fetched = engine
        .fetch_external(resident.item_id)
        .await
        .unwrap()
        .expect("fetch returns the catalog body");
    assert!(
        fetched.content.contains("AuthService"),
        "Resident fetch must return the heap body, not claim it is already in the working set"
    );
    assert_eq!(fetched.residency, ContextResidency::Resident);
}

#[tokio::test]
async fn exact_context_uri_and_path_substrings_survive_unrelated_text_candidates() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut target = crate::item::make_item(
            &state,
            &engine.config,
            "unrelated target body".into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        target.entities.clear();
        target.file_path = Some("src/oauth_handler.rs".into());
        target.file_revision = Some("rev-1".into());
        let target_id = target.id;
        state.items.push(target);

        let mut distractor = crate::item::make_item(
            &state,
            &engine.config,
            "context auth noise".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        distractor.entities.clear();
        state.items.push(distractor);
        target_id
    };

    let uri = format!("context://run/{target_id}");
    let uri_hits = engine
        .search_external(ContextSearchQuery::new(uri, 8))
        .await
        .unwrap();
    assert_eq!(
        uri_hits.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![target_id],
        "the stable URI must be indexed even when another text candidate exists"
    );

    let path_hits = engine
        .search_external(ContextSearchQuery::new("auth_hand", 8))
        .await
        .unwrap();
    assert_eq!(
        path_hits.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![target_id],
        "path substring lookup must not depend on a duplicate entity signature"
    );
}

#[tokio::test]
async fn live_fs_read_stamps_path_and_is_a_catalog_search_hit() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix src/auth/login.rs").await;
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "     1 | fn handle_21() {}".into(),
                artifact_ref: None,
                metadata: serde_json::json!({
                    "path": "src/auth/login.rs",
                    "revision": "abc",
                }),
            },
            scope_id: None,
        })
        .await
        .unwrap();

    let item_id = {
        let state = engine.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.source.as_deref() == Some("tool:fs.read"))
            .expect("fs.read observation is ingested");
        assert_eq!(item.file_path.as_deref(), Some("src/auth/login.rs"));
        assert_eq!(item.file_revision.as_deref(), Some("abc"));
        assert!(
            item.entities
                .iter()
                .any(|entity| entity == "src/auth/login.rs"),
            "catalog entity index must include the stamped path: {:?}",
            item.entities
        );
        assert!(
            state.latest_file_body_ids().contains(&item.id),
            "numbered-line reads with metadata.path are latest-file-body roots"
        );
        item.id
    };

    let hits = engine
        .search_external(ContextSearchQuery::new("src/auth/login.rs", 8))
        .await
        .unwrap();
    assert!(
        hits.iter()
            .any(|hit| hit.item_id == item_id
                && hit.file_path.as_deref() == Some("src/auth/login.rs")),
        "path-based catalog search must hit a live fs.read: {hits:?}"
    );
}

/// Search and inspect return identity for raw file/tool evidence. The
/// 120-char body prefix stays on the store map for GC; the model-facing
/// card is `path@rev`. Fetch still returns the catalog body.
#[tokio::test]
async fn search_and_inspect_file_body_are_identity_descriptors() {
    const BODY: &str = "fn handle_secret_search() {}";
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
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
                metadata: serde_json::json!({
                    "path": "AuthService.rs",
                    "revision": "abc",
                }),
            },
            scope_id: None,
        })
        .await
        .unwrap();

    let hits = engine
        .search_external(ContextSearchQuery::new("AuthService.rs", 8))
        .await
        .unwrap();
    let hit = hits
        .iter()
        .find(|hit| hit.file_path.as_deref() == Some("AuthService.rs"))
        .expect("path search still hits a live fs.read");
    assert_eq!(hit.context_ref.summary, "AuthService.rs@abc");
    assert!(
        !hit.context_ref.summary.contains(BODY),
        "search must not dump the file text: {}",
        hit.context_ref.summary
    );

    let inspected = engine
        .inspect_external(hit.item_id)
        .await
        .unwrap()
        .expect("inspect returns a descriptor");
    assert_eq!(inspected.context_ref.summary, "AuthService.rs@abc");
    assert!(
        !inspected.context_ref.summary.contains(BODY),
        "inspect must not dump the file text"
    );

    let body_hits = engine
        .search_external(ContextSearchQuery::new("handle_secret_search", 8))
        .await
        .unwrap();
    assert!(
        body_hits
            .iter()
            .all(|hit| hit.file_path.as_deref() != Some("AuthService.rs")),
        "file text is not a search needle: {body_hits:?}"
    );

    let fetched = engine
        .fetch_external(hit.item_id)
        .await
        .unwrap()
        .expect("Fetch still returns the catalog body");
    assert!(
        fetched.content.contains(BODY),
        "exact body stays behind Fetch"
    );
}

#[tokio::test]
async fn search_hits_stamp_a_bounded_recency_reinforcement() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // 种入 10 个外部条目：content 不含大写/特殊字符，entities 为空，
    // 排序键只剩 last_access_tick（= 外部化 tick i），hits 顺序确定。
    {
        let mut state = engine.state.lock().await;
        let config = SimpleContextConfig::default();
        for i in 0..10u64 {
            let mut item = crate::item::make_item(
                &state,
                &config,
                format!("item-{i} finding"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.id = ContextItemId::new();
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            state.external.push(crate::store::to_external_entry(
                &item, reference, i, 0, None,
            ));
        }
    }

    let before = engine.state.lock().await.event_seq;
    let hits = engine
        .search_external(ContextSearchQuery::new("item", 10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 10, "every seeded entry matches the query");

    // search 返回排序后的 clone（命中顺序 = 外部化 tick 降序 9..0），
    // recency 强化是内部副作用，只能从 state.external 观察：前 8 个命中
    // （tick 9..2）的 last_access 时钟被刷新，超出上限的（tick 1/0）不动。
    let state = engine.state.lock().await;
    for entry in state.external.iter() {
        let tick = entry.externalized_at_tick;
        if tick >= 2 {
            assert_eq!(
                entry.last_access_tick, before,
                "命中（tick {tick}）必须被刷新 recency"
            );
            assert_eq!(
                entry.last_access_gc_epoch,
                Some(state.gc_epoch),
                "命中（tick {tick}）必须锚定当前 GC 世代"
            );
            assert_eq!(
                entry.last_access_signal,
                AccessSignal::SearchHit,
                "命中（tick {tick}）必须记为最弱 search 信号"
            );
            assert_eq!(entry.search_reinforce_count, 1);
        } else {
            assert_eq!(
                entry.last_access_tick, tick,
                "超出强化上限的命中（tick {tick}）必须保持原时钟"
            );
            assert_eq!(entry.last_access_signal, AccessSignal::None);
            assert_eq!(entry.search_reinforce_count, 0);
        }
    }
    assert_eq!(
        state.access_search_hits, 8,
        "diagnostics 计的是落地的 search 戳，不是描述符行数"
    );
}

#[tokio::test]
async fn search_reinforcement_delays_cold_to_external_aging() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_external_ttl_generations: 2,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // 两个外部条目都锚定在 epoch 0（Cold）；A 的 summary 含 "alpha"，
    // B 不含，所以 "alpha" 只会命中 A。
    let (id_a, id_b) = {
        let mut state = engine.state.lock().await;
        let config = SimpleContextConfig::default();
        let mut seed = |content: &str, tick: u64| {
            let mut item = crate::item::make_item(
                &state,
                &config,
                content.to_string(),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.id = ContextItemId::new();
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            state.external.push(crate::store::to_external_entry(
                &item, reference, tick, 0, None,
            ));
            item.id
        };
        (seed("alpha finding", 0), seed("beta finding", 1))
    };

    // 第一次完整 GC：epoch 1，两个条目 idle = 1 < ttl 2，都保持 Cold。
    let report = engine.gc().await.unwrap();
    assert_eq!(report.aged_external, 0, "首轮 GC 必须没有条目老化");
    {
        let state = engine.state.lock().await;
        assert_eq!(state.gc_epoch, 1);
        for entry in state.external.iter() {
            assert_eq!(entry.residency, ContextResidency::Cold);
        }
    }

    // search 只命中 A，把它锚定到当前世代（1）。
    let hits = engine
        .search_external(ContextSearchQuery::new("alpha", 10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "只有 A 匹配 alpha");
    assert_eq!(hits[0].item_id, id_a);
    {
        let state = engine.state.lock().await;
        let a = state
            .external
            .iter()
            .find(|e| e.item_id == id_a)
            .expect("A 仍在外部映射中");
        assert_eq!(
            a.last_access_gc_epoch,
            Some(1),
            "search 命中必须刷新 A 的世代锚点"
        );
    }

    // 第二次完整 GC：epoch 2。A idle = 1 < 2 保持 Cold；B idle = 2 >= 2
    // 正常降级 External——search 的强化延迟了老化，但不会让条目永生。
    let report = engine.gc().await.unwrap();
    assert_eq!(report.aged_external, 1, "未被强化的 B 必须老化");
    let state = engine.state.lock().await;
    let a = state
        .external
        .iter()
        .find(|e| e.item_id == id_a)
        .expect("A 仍在外部映射中");
    let b = state
        .external
        .iter()
        .find(|e| e.item_id == id_b)
        .expect("B 仍在外部映射中");
    assert_eq!(
        a.residency,
        ContextResidency::Cold,
        "被 search 强化的条目必须延缓 Cold -> External 老化"
    );
    assert_eq!(
        b.residency,
        ContextResidency::External,
        "未被强化的条目按 ttl 正常老化"
    );
}

#[tokio::test]
async fn identical_search_query_budget_blocks_a_second_stamp_in_the_same_turn() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &SimpleContextConfig::default(),
            "item finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 0, 0, None,
        ));
        item.id
    };

    engine
        .search_external(ContextSearchQuery::new("item", 8))
        .await
        .unwrap();
    let first_tick = {
        let state = engine.state.lock().await;
        state.external.get(item_id).unwrap().last_access_tick
    };

    // Pin 推进 event_seq，但不结束用户回合，相同查询预算不得重置。
    engine
        .ingest(ContextIngress::Pin {
            content: "keep the budget".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    engine
        .search_external(ContextSearchQuery::new("item", 8))
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let entry = state.external.get(item_id).unwrap();
    assert_eq!(
        entry.last_access_tick, first_tick,
        "identical query must not restamp recency inside one turn"
    );
    assert_eq!(entry.last_access_signal, AccessSignal::SearchHit);
}

#[tokio::test]
async fn search_hit_cools_down_inside_the_same_event_seq() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &SimpleContextConfig::default(),
            "alpha finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 0, 0, None,
        ));
        item.id
    };

    engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    let first = {
        let state = engine.state.lock().await;
        let entry = state.external.get(item_id).unwrap();
        (entry.last_access_tick, entry.search_reinforce_count)
    };

    engine
        .search_external(ContextSearchQuery::new("finding", 8))
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let entry = state.external.get(item_id).unwrap();
    assert_eq!(
        entry.last_access_tick, first.0,
        "a second search in the same event_seq must cool down per item"
    );
    assert_eq!(entry.search_reinforce_count, first.1);
}

#[tokio::test]
async fn inspect_outranks_search_and_resets_saturation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &SimpleContextConfig::default(),
            "alpha finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 0, 0, None,
        ));
        item.id
    };

    engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let entry = state.external.get(item_id).unwrap();
        assert_eq!(entry.last_access_signal, AccessSignal::SearchHit);
        assert_eq!(entry.search_reinforce_count, 1);
    }

    let inspected = engine.inspect_external(item_id).await.unwrap().unwrap();
    assert_eq!(inspected.last_access_signal, AccessSignal::Inspect);
    assert_eq!(
        inspected.search_reinforce_count, 0,
        "inspect must reset search saturation so a later stronger path can delay aging"
    );

    engine
        .ingest(ContextIngress::Pin {
            content: "advance seq".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let entry = state.external.get(item_id).unwrap();
    assert_eq!(
        entry.last_access_signal,
        AccessSignal::Inspect,
        "search must not overwrite a stronger inspect signal"
    );
}

#[tokio::test]
async fn search_saturation_cannot_pin_cold_entries_across_gc_passes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_external_ttl_generations: 2,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let (id_a, id_b) = {
        let mut state = engine.state.lock().await;
        let config = SimpleContextConfig::default();
        let mut seed = |content: &str, tick: u64| {
            let mut item = crate::item::make_item(
                &state,
                &config,
                content.to_string(),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.id = ContextItemId::new();
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            state.external.push(crate::store::to_external_entry(
                &item, reference, tick, 0, None,
            ));
            item.id
        };
        (seed("alpha finding", 0), seed("beta finding", 1))
    };

    engine.gc().await.unwrap();
    engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.get(id_a).unwrap().residency,
            ContextResidency::Cold,
            "the first search still delays aging once (CTX-GC-10)"
        );
        assert_eq!(
            state.external.get(id_b).unwrap().residency,
            ContextResidency::External
        );
    }

    // 新回合重置相同查询预算，但条目已饱和：search 不得再刷新 gc_epoch。
    engine
        .ingest(ContextIngress::UserMessage {
            content: "keep looking".into(),
        })
        .await
        .unwrap();
    engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.get(id_a).unwrap().last_access_gc_epoch,
            Some(1),
            "saturated search must not refresh the Cold aging anchor"
        );
    }

    engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert_eq!(
        state.external.get(id_a).unwrap().residency,
        ContextResidency::External,
        "search must not become a hidden pin API"
    );
}

#[tokio::test]
async fn terminal_external_entries_are_hidden_from_every_retrieval_surface() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });

    let (live_id, terminal_ids) = {
        let mut state = engine.state.lock().await;
        let semantics = [
            SemanticState::Live,
            SemanticState::Superseded { by: None },
            SemanticState::VerifiedFixed { by: None },
            SemanticState::Tombstoned,
        ];
        let mut ids = Vec::new();
        for (offset, semantic) in semantics.into_iter().enumerate() {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("RecallSurface.rs entry {offset}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.semantic = semantic;
            item.entities = crate::index::entity::extract_entities(&item.content);
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            let mut entry =
                crate::store::to_external_entry(&item, reference, offset as u64 + 1, 1, None);
            if matches!(semantic, SemanticState::VerifiedFixed { .. }) {
                entry.residency = agent_contracts::ContextResidency::External;
            }
            state.external.push(entry);
            ids.push(item.id);
        }
        (ids[0], ids[1..].to_vec())
    };

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "RecallSurface.rs".into(),
            budget_tokens: 1_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        materialized
            .external
            .iter()
            .map(|entry| entry.item_id)
            .collect::<Vec<_>>(),
        vec![live_id],
        "the materialized external view must expose only live refs"
    );

    let hits = engine
        .search_external(ContextSearchQuery::new("RecallSurface", 10))
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|entry| entry.item_id).collect::<Vec<_>>(),
        vec![live_id],
        "search must not return semantically terminal refs"
    );

    for item_id in terminal_ids {
        assert!(
            engine.inspect_external(item_id).await.unwrap().is_none(),
            "inspect must hide terminal ref {item_id:?}"
        );
        assert!(
            engine.fetch_external(item_id).await.unwrap().is_none(),
            "fetch must refuse terminal ref {item_id:?}"
        );
    }
}

#[tokio::test]
async fn external_terminal_transitions_refresh_catalog_live_and_attention_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let (decision_id, error_id) = {
        let mut state = engine.state.lock().await;
        let mut ids = Vec::new();
        for (kind, content) in [
            (ContextKind::Decision, "TerminalCatalog.rs old decision"),
            (ContextKind::Error, "TerminalError.rs fixed failure"),
        ] {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                content.into(),
                kind,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.entities = crate::index::entity::extract_entities(&item.content);
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            state.external.push(crate::store::to_external_entry(
                &item, reference, 1, 1, None,
            ));
            ids.push(item.id);
        }
        (ids[0], ids[1])
    };

    let query = ContextSearchQuery::new("Terminal", 8);
    let before = engine.search_external(query.clone()).await.unwrap();
    assert_eq!(before.len(), 2, "seed the live catalog before mutation");

    {
        let mut state = engine.state.lock().await;
        state.pending_supersessions.push((
            decision_id,
            ContextItemId::new(),
            "newer decision".into(),
        ));
        state.pending_verifications.push((
            error_id,
            ContextItemId::new(),
            "successful verifier".into(),
        ));
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let after = engine.search_external(query.clone()).await.unwrap();
    assert!(after.is_empty(), "terminal entries must leave live search");
    let mut state = engine.state.lock().await;
    state.sync_catalog();
    let candidates = state
        .catalog
        .search_candidates(&query)
        .expect("indexed terminal text yields a bounded empty candidate set");
    assert!(
        candidates.ids.is_empty(),
        "the catalog live set must not retain terminal ids"
    );
    for id in [decision_id, error_id] {
        assert!(
            state
                .catalog
                .ids_for_attention(AttentionState::Archived)
                .contains(&id),
            "terminal transition must move {id} into the archived bucket"
        );
    }
}

#[tokio::test]
async fn stored_semantic_search_verifies_keywords_beyond_the_descriptor_summary() {
    let dir = tempfile::tempdir().unwrap();
    let engine = std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }));
    let item_id = {
        let mut state = engine.state.lock().await;
        let content = format!(
            "{} deep_middle_decision_token {}",
            "prefix ".repeat(100),
            "suffix ".repeat(100)
        );
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            content,
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        item.entities.clear();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        assert!(
            !reference.summary.contains("deep_middle_decision_token"),
            "the fixture must hide the needle beyond the stored descriptor"
        );
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        item.id
    };

    let gate = engine.op_gate.lock().await;
    let search = {
        let engine = std::sync::Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .search_external(ContextSearchQuery::new("deep_middle_decision_token", 8))
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !search.is_finished(),
        "a Stored full-text read must wait for the operation gate"
    );
    drop(gate);
    let hits = search.await.unwrap().unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![item_id],
        "an explicit search must checked-read an incomplete Stored semantic body"
    );
    let state = engine.state.lock().await;
    assert!(state.external.get(item_id).is_some());
    assert!(state.items.iter().all(|item| item.id != item_id));
}

#[tokio::test]
async fn stored_semantic_search_rejects_a_checksum_mismatched_body() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("{} checksum_search_token", "prefix ".repeat(100)),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        item.entities.clear();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item,
            reference,
            1,
            1,
            Some("not-the-blob-checksum".into()),
        ));
    }

    let error = engine
        .search_external(ContextSearchQuery::new("checksum_search_token", 8))
        .await
        .expect_err("an unowned/tampered body must never satisfy search");
    assert!(
        error.to_string().contains("corrupt"),
        "checksum failure must remain typed in the search error: {error}"
    );
}

#[tokio::test]
async fn stored_search_rejects_an_oversized_replaced_blob_before_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "bounded stored descriptor".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        item.entities.clear();
        let reference = crate::store::make_context_ref(&item);
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        item.id
    };
    std::fs::write(
        dir.path().join(format!("{item_id}.json")),
        vec![b'x'; crate::store::MAX_CONTEXT_STORE_BLOB_BYTES + 1],
    )
    .unwrap();

    let error = engine
        .search_external(ContextSearchQuery::new("absent_deep_token", 8))
        .await
        .expect_err("an oversized substituted body must fail before decode");
    assert!(
        error.to_string().contains("corrupt"),
        "the byte ceiling must surface as an integrity failure: {error}"
    );
}

#[tokio::test]
async fn stored_semantic_search_reports_a_missing_planned_body() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("{} missing_search_token", "prefix ".repeat(100)),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        item.entities.clear();
        let reference = crate::store::make_context_ref(&item);
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
    }

    let error = engine
        .search_external(ContextSearchQuery::new("missing_search_token", 8))
        .await
        .expect_err("an unreadable planned body cannot become a complete miss");
    assert!(
        error.to_string().contains("missing"),
        "the search error must preserve the missing-body class: {error}"
    );
}

#[tokio::test]
async fn stored_search_rejects_a_semantic_descriptor_with_a_raw_body() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut owner = crate::item::make_item(
            &state,
            &engine.config,
            "semantic descriptor filler ".repeat(40),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        owner.entities.clear();
        let reference = crate::store::externalize(dir.path(), &owner).unwrap();
        state.external.push(crate::store::to_external_entry(
            &owner, reference, 1, 1, None,
        ));

        let mut substituted = owner.clone();
        substituted.kind = ContextKind::ToolObservation;
        substituted.content = "raw_kind_bypass_token".into();
        substituted.file_path = Some("src/substituted.rs".into());
        substituted.file_revision = Some("rev-1".into());
        std::fs::write(
            dir.path().join(format!("{}.json", owner.id)),
            serde_json::to_vec(&substituted).unwrap(),
        )
        .unwrap();
        owner.id
    };

    let error = engine
        .search_external(ContextSearchQuery::new("raw_kind_bypass_token", 8))
        .await
        .expect_err("descriptor/body kind drift must fail closed");
    let message = error.to_string();
    assert!(
        message.contains("corrupt") && message.contains("kind"),
        "the mismatch must remain explicit for {item_id}: {message}"
    );
}

#[tokio::test]
async fn unindexed_short_and_cjk_queries_check_resident_and_stored_semantic_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let (resident_id, stored_id) = {
        let mut state = engine.state.lock().await;
        let mut resident = crate::item::make_item(
            &state,
            &engine.config,
            format!("{}你好世界", "resident prefix ".repeat(12)),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        resident.entities.clear();
        let resident_id = resident.id;
        state.items.push(resident);

        let mut stored = crate::item::make_item(
            &state,
            &engine.config,
            format!("{}你好世界", "stored prefix ".repeat(12)),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        stored.entities.clear();
        let stored_id = stored.id;
        let reference = crate::store::externalize(dir.path(), &stored).unwrap();
        assert!(!reference.summary.contains('界'));
        state.external.push(crate::store::to_external_entry(
            &stored, reference, 1, 1, None,
        ));
        (resident_id, stored_id)
    };

    for query in ["界", "世界"] {
        let hits = engine
            .search_external(ContextSearchQuery::new(query, 8))
            .await
            .unwrap();
        let ids: std::collections::HashSet<_> = hits.iter().map(|hit| hit.item_id).collect();
        assert_eq!(
            ids,
            [resident_id, stored_id].into_iter().collect(),
            "query {query:?} must not change meaning when a body moves to Stored"
        );
    }
}

#[tokio::test]
async fn ascii_infix_body_query_has_the_same_token_semantics_in_every_residency() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let mut resident = crate::item::make_item(
            &state,
            &engine.config,
            format!("{}AuthService", "resident prefix ".repeat(12)),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        resident.entities.clear();
        state.items.push(resident);

        let mut stored = crate::item::make_item(
            &state,
            &engine.config,
            format!("{}AuthService", "stored prefix ".repeat(12)),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        stored.entities.clear();
        let reference = crate::store::externalize(dir.path(), &stored).unwrap();
        assert!(!reference.summary.to_lowercase().contains("ervice"));
        state.external.push(crate::store::to_external_entry(
            &stored, reference, 1, 1, None,
        ));
    }

    let hits = engine
        .search_external(ContextSearchQuery::new("ervice", 8))
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "semantic bodies use exact-token matching, not arbitrary ASCII token infixes"
    );
}

#[tokio::test]
async fn ambiguous_ascii_prefix_is_not_a_body_match_in_any_residency() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let mut resident = crate::item::make_item(
            &state,
            &engine.config,
            format!("{} authentication", "semantic prefix ".repeat(12)),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        resident.entities.clear();
        state.items.push(resident);

        let mut stored = crate::item::make_item(
            &state,
            &engine.config,
            format!("{} author", "stored prefix ".repeat(12)),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        stored.entities.clear();
        let reference = crate::store::externalize(dir.path(), &stored).unwrap();
        state.external.push(crate::store::to_external_entry(
            &stored, reference, 1, 1, None,
        ));
    }

    let hits = engine
        .search_external(ContextSearchQuery::new("auth", 8))
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "ambiguous prefixes are descriptor candidates, not exact semantic-body tokens"
    );
}

#[tokio::test]
async fn query_tokens_after_the_document_budget_are_required_at_verification() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let tokens = (0..65).map(|i| format!("{i:02x}")).collect::<Vec<_>>();
    let query = tokens.join(" ");
    assert!(query.chars().count() <= CONTEXT_SEARCH_MAX_QUERY_CHARS);
    let expected_id = {
        let mut state = engine.state.lock().await;
        for body in [tokens[..64].join(" "), query.clone()] {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                body,
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.4,
                None,
            );
            item.entities.clear();
            state.items.push(item);
        }
        state.items[1].id
    };

    let hits = engine
        .search_external(ContextSearchQuery::new(query, 8))
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![expected_id],
        "a body missing query token 65 must not pass the final AND check"
    );
}

#[tokio::test]
async fn mixed_short_and_cjk_query_fragments_are_not_silently_dropped() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let (ascii_target, cjk_target, cyrillic_target) = {
        let mut state = engine.state.lock().await;
        let mut ids = Vec::new();
        for content in ["a zebra", "界 zebra", "zebra", "кот", "скот"] {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                content.into(),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.4,
                None,
            );
            item.entities.clear();
            ids.push(item.id);
            state.items.push(item);
        }
        (ids[0], ids[1], ids[3])
    };

    let ascii = engine
        .search_external(ContextSearchQuery::new("a zebra", 8))
        .await
        .unwrap();
    assert_eq!(
        ascii.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![ascii_target],
        "the one-character ASCII token must remain part of final verification"
    );

    let cjk = engine
        .search_external(ContextSearchQuery::new("界 zebra", 8))
        .await
        .unwrap();
    assert_eq!(
        cjk.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![cjk_target],
        "the CJK fragment and the ASCII token must both be present"
    );

    let cyrillic = engine
        .search_external(ContextSearchQuery::new("кот", 8))
        .await
        .unwrap();
    assert_eq!(
        cyrillic.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![cyrillic_target],
        "Unicode alphabets with stable token boundaries remain exact"
    );
}

#[tokio::test]
async fn stored_raw_evidence_body_is_fetch_only_even_during_full_text_residuals() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let raw_id = {
        let mut state = engine.state.lock().await;
        let mut raw = crate::item::make_item(
            &state,
            &engine.config,
            format!("{} fetch_only_raw_secret", "raw ".repeat(150)),
            ContextKind::ToolObservation,
            ContextScope::Turn,
            ContextRetention::Working,
            0.4,
            Some("tool:fs.read".into()),
        );
        raw.entities.clear();
        raw.file_path = Some("src/raw-evidence.rs".into());
        raw.file_revision = Some("rev-1".into());
        let raw_ref = crate::store::externalize(dir.path(), &raw).unwrap();
        state
            .external
            .push(crate::store::to_external_entry(&raw, raw_ref, 1, 1, None));

        // A semantic Stored body makes this query explicitly incomplete and
        // forces the residual IO path. It does not contain the raw secret.
        let mut semantic = crate::item::make_item(
            &state,
            &engine.config,
            "semantic filler ".repeat(100),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        semantic.entities.clear();
        let semantic_ref = crate::store::externalize(dir.path(), &semantic).unwrap();
        state.external.push(crate::store::to_external_entry(
            &semantic,
            semantic_ref,
            2,
            1,
            None,
        ));
        raw.id
    };
    // If search ever reads the raw evidence blob, its corruption becomes a
    // hard error. The successful empty result therefore proves the raw body
    // was excluded from the residual read plan as well as final matching.
    std::fs::write(dir.path().join(format!("{raw_id}.json")), b"corrupt").unwrap();

    let hidden = engine
        .search_external(ContextSearchQuery::new("fetch_only_raw_secret", 8))
        .await
        .unwrap();
    assert!(
        hidden.is_empty(),
        "raw tool body must not be a search needle"
    );

    let identity = engine
        .search_external(ContextSearchQuery::new("src/raw-evidence.rs@rev-1", 8))
        .await
        .unwrap();
    assert_eq!(
        identity.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![raw_id],
        "path@revision identity remains searchable without reading the raw blob"
    );
}

#[tokio::test]
async fn pinned_raw_body_entities_stay_unsearchable_after_checkpoint_restore() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::Pin {
            content: "fetch_only_secret.rs SecretBodyToken".into(),
            kind: ContextKind::ToolObservation,
        })
        .await
        .unwrap();
    let raw_id = {
        let state = engine.state.lock().await;
        let raw = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("the explicit raw pin exists");
        assert!(
            raw.entities.is_empty(),
            "raw construction must not derive search entities from its body"
        );
        raw.id
    };

    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint).await.unwrap();
    {
        let state = engine.state.lock().await;
        let raw = state.items.iter().find(|item| item.id == raw_id).unwrap();
        assert!(
            raw.entities.is_empty(),
            "legacy entity backfill must not reinterpret a raw body as descriptor metadata"
        );
    }
    // Simulate an older checkpoint that already persisted body-derived raw
    // entities. Catalog construction and final projection must sanitize those
    // too; merely skipping the empty-entity backfill is not enough.
    {
        let mut state = engine.state.lock().await;
        let index = state.items.indexes().get(raw_id).unwrap();
        state.items.update_entities(
            index,
            vec!["fetch_only_secret.rs".into(), "SecretBodyToken".into()],
        );
    }
    let legacy_checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(legacy_checkpoint).await.unwrap();
    for query in ["fetch_only_secret.rs", "SecretBodyToken"] {
        let hits = engine
            .search_external(ContextSearchQuery::new(query, 8))
            .await
            .unwrap();
        assert!(
            hits.iter().all(|hit| hit.item_id != raw_id),
            "raw body-derived entity {query:?} must stay Fetch-only"
        );
    }
}

#[tokio::test]
async fn direct_engine_search_enforces_query_and_result_bounds() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let long_query_target = {
        let mut state = engine.state.lock().await;
        for index in 0..(CONTEXT_SEARCH_MAX_LIMIT + 10) {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("bounded_result_token row {index}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.4,
                None,
            );
            item.entities.clear();
            state.items.push(item);
        }
        let mut target = crate::item::make_item(
            &state,
            &engine.config,
            "q".repeat(CONTEXT_SEARCH_MAX_QUERY_CHARS),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        target.entities.clear();
        let id = target.id;
        state.items.push(target);
        id
    };

    let bounded = engine
        .search_external(ContextSearchQuery::new("bounded_result_token", usize::MAX))
        .await
        .unwrap();
    assert_eq!(
        bounded.len(),
        CONTEXT_SEARCH_MAX_LIMIT,
        "direct engine callers cannot request an unbounded result set"
    );

    let truncated = engine
        .search_external(ContextSearchQuery::new(
            "q".repeat(CONTEXT_SEARCH_MAX_QUERY_CHARS + 40),
            1,
        ))
        .await
        .unwrap();
    assert_eq!(
        truncated.iter().map(|hit| hit.item_id).collect::<Vec<_>>(),
        vec![long_query_target],
        "direct engine queries use the same character cap as the Core path"
    );
}

#[tokio::test]
async fn stored_full_text_search_refuses_an_unbounded_body_scan() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        for index in 0..=crate::store::MAX_STORED_SEARCH_READS {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("stored semantic filler {index}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.4,
                None,
            );
            item.entities.clear();
            let reference = crate::store::make_context_ref(&item);
            state.external.push(crate::store::to_external_entry(
                &item, reference, 1, 1, None,
            ));
        }
    }

    let error = engine
        .search_external(ContextSearchQuery::new("absent_deep_needle", 8))
        .await
        .expect_err("a broad disk scan must fail explicitly at its work bound");
    assert!(
        error
            .to_string()
            .contains(&crate::store::MAX_STORED_SEARCH_READS.to_string()),
        "the refinable error must name the Stored read cap: {error}"
    );
}

#[tokio::test]
async fn inspect_is_bounded_across_the_logical_catalog() {
    // The catalog spans the heap and the external store. With a limit, the
    // call must return only the `limit` smallest created ticks — and keep
    // that cost bounded no matter how large the store's history grows.
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });

    // Resident items with controlled created ticks.
    for (offset, tick) in [30u64, 10, 50].into_iter().enumerate() {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("resident {offset}"),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.created_tick = tick;
        state.items.push(item);
    }
    // External store entries with interleaved ticks: the entry now carries
    // the item's real creation clock, so the fixture sets it explicitly
    // (the externalized_at_tick argument stays for the store metadata).
    for (offset, tick) in [20u64, 5, 40].into_iter().enumerate() {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            format!("external {offset}"),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.created_tick = tick;
        item.entities = crate::index::entity::extract_entities(&item.content);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        let entry = crate::store::to_external_entry(&item, reference, tick, 1, None);
        state.external.push(entry);
    }

    let kept = engine.inspect(3).await.unwrap();
    assert_eq!(kept.len(), 3, "the limit caps the returned rows");
    let ticks: Vec<u64> = kept.iter().map(|summary| summary.created_tick).collect();
    assert_eq!(
        ticks,
        vec![5, 10, 20],
        "the three smallest ticks across the heap and the store, ascending"
    );

    let all = engine.inspect(usize::MAX).await.unwrap();
    assert_eq!(all.len(), 6, "no limit keeps the whole logical catalog");
    let all_ticks: Vec<u64> = all.iter().map(|summary| summary.created_tick).collect();
    assert_eq!(
        all_ticks,
        vec![5, 10, 20, 30, 40, 50],
        "the full catalog is ascending by created tick"
    );
}

#[tokio::test]
async fn consumption_ack_stamps_an_external_descriptor_without_reactivating_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "ExternalAck.rs historical decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.6,
            Some("tool-session".into()),
        );
        item.entities = crate::index::entity::extract_entities(&item.content);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 0, 0, None,
        ));
        item.id
    };

    let preview = engine
        .materialize(ContextQuery {
            current_input: "inspect ExternalAck.rs".into(),
            budget_tokens: 2_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        preview
            .external
            .iter()
            .map(|entry| entry.item_id)
            .collect::<Vec<_>>(),
        vec![item_id]
    );
    let before_tick = {
        let state = engine.state.lock().await;
        state
            .external
            .get(item_id)
            .expect("descriptor is stored")
            .last_access_tick
    };
    assert_eq!(before_tick, 0, "preview must not stamp access");

    acknowledge_all(&engine, &preview).await;

    let after = engine.inspect_external(item_id).await.unwrap().unwrap();
    assert!(after.last_access_tick > before_tick);
    assert_eq!(after.last_access_gc_epoch, Some(0));
    assert_eq!(
        after.last_access_signal,
        AccessSignal::ConsumptionAck,
        "inspect after ack must not downgrade the strongest signal"
    );
    assert_eq!(after.access_count, 1);
    assert_eq!(after.last_selected_turn, after.last_access_turn);
    // The acknowledged descriptor stays in the logical catalog, but only as
    // an external projection — acknowledging must not page its body back
    // into the resident heap.
    let catalog = engine.inspect(usize::MAX).await.unwrap();
    let entry = catalog
        .iter()
        .find(|item| item.id == item_id)
        .expect("the acknowledged descriptor stays part of the logical catalog");
    assert_eq!(
        entry.source.as_deref(),
        Some("tool-session"),
        "acknowledging a descriptor must not page its body back into memory, and the source authority survives"
    );
}

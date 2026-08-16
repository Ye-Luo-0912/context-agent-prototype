use agent_contracts::{
    AccessSignal, ContextEngine, ContextHints, ContextIngress, ContextItemId, ContextKind,
    ContextQuery, ContextResidency, ContextRetention, ContextScope, ContextSearchQuery, CoreLabel,
    Label, SemanticState, TaskId, ToolOutput,
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
    assert!(
        engine
            .fetch_external(resident.item_id)
            .await
            .unwrap()
            .is_none(),
        "fetch remains a store read; resident bodies stay in the working set"
    );
}

#[tokio::test]
async fn live_fs_read_stamps_path_and_is_a_catalog_search_hit() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix src/auth/login.rs").await;
    engine
        .ingest(ContextIngress::ToolObservation {
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

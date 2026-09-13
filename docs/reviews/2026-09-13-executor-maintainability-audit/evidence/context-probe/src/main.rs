//! Read-only audit of production sources. Fixtures use validated ContextEngine
//! checkpoints to place a body in an existing owner state; no product code edits.
use agent_contracts::*;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::{Value, json};

async fn seeded(config: SimpleContextConfig) -> (SimpleContextEngine, Value, Value) {
    let engine = SimpleContextEngine::new(config);
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "audit fixture"),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "seed".into(),
        })
        .await
        .unwrap();
    let mut cp = engine.checkpoint().await.unwrap();
    let mut item = cp["items"].as_array().unwrap().last().unwrap().clone();
    cp["items"] = json!([]);
    item["kind"] = serde_json::to_value(ContextKind::Note).unwrap();
    item["attention"] = serde_json::to_value(AttentionState::Archived).unwrap();
    item["residency"] = serde_json::to_value(ContextResidency::Warm).unwrap();
    item["evicted_at_tick"] = json!(0);
    item["scope_id"] = Value::Null;
    item["importance"] = json!(0.0);
    item["relevance"] = json!(0.0);
    item["entities"] = json!([]);
    item["tags"] = json!([]);
    (engine, cp, item)
}

fn query(path: &str) -> ContextQuery {
    ContextQuery {
        current_input: format!("inspect {path}"),
        budget_tokens: 10000,
        hints: ContextHints {
            foreground_resources: vec![ResourceKey {
                path: path.into(),
                revision: Some("rev-1".into()),
            }],
            ..Default::default()
        },
    }
}

async fn pending_foreground() -> Value {
    let (engine, mut cp, mut item) = seeded(SimpleContextConfig::default()).await;
    item["kind"] = serde_json::to_value(ContextKind::ToolObservation).unwrap();
    item["source"] = json!("fs.read");
    item["file_path"] = json!("src/a.rs");
    item["file_revision"] = json!("rev-1");
    item["file_start_line"] = json!(1);
    item["file_end_line"] = json!(1);
    item["content"] = json!("1 | fn a() {}\n");
    cp["pending_externalize_retry"] = json!([item.clone()]);
    engine.restore(cp).await.unwrap();
    let id: ContextItemId = serde_json::from_value(item["id"].clone()).unwrap();
    let fetch = engine.fetch_external(id).await.unwrap().is_some();
    let m = engine.materialize(query("src/a.rs")).await.unwrap();
    json!({"case":"pending_foreground", "fetch_has_body":fetch,
        "foreground_count":m.foreground.len(), "misses":m.optional_misses})
}

async fn warm_recall_budget() -> Value {
    let (engine, mut cp, mut target) = seeded(SimpleContextConfig {
        gc_reactivate_per_pass: 1,
        ..Default::default()
    })
    .await;
    target["content"] = json!("needed context about AuthService.rs");
    target["entities"] = json!(["AuthService.rs"]);
    let target_id = target["id"].clone();
    let mut decoy = target.clone();
    decoy["id"] = json!(ContextItemId::new());
    decoy["content"] = json!("unrelated shell output");
    decoy["kind"] = serde_json::to_value(ContextKind::ToolObservation).unwrap();
    decoy["entities"] = json!([]);
    decoy["source"] = json!("shell.exec");
    cp["eviction_buffer"] = json!([target, decoy]);
    cp["user_hot_entities"] = json!(["AuthService.rs"]);
    cp["hot_entities"] = json!(["AuthService.rs"]);
    engine.restore(cp).await.unwrap();
    let mut recalls = Vec::new();
    for _ in 0..3 {
        recalls.push(engine.gc().await.unwrap().reactivated);
    }
    let after = engine.checkpoint().await.unwrap();
    json!({"case":"warm_recall_budget", "reactivations":recalls,
        "target_still_warm":after["eviction_buffer"].as_array().unwrap().iter().any(|x|x["id"]==target_id)})
}

async fn retirement_ring() -> Value {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 3,
        ..Default::default()
    });
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "fill ring"),
        })
        .await
        .unwrap();
    let mut first = None;
    let mut last = None;
    for i in 0..513 {
        let id = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
        if i == 0 {
            first = Some(id);
        }
        last = Some(id);
        engine.close_scope(id).await.unwrap();
        engine.gc().await.unwrap();
    }
    let cp = engine.checkpoint().await.unwrap();
    let notes = cp["retired_scopes"].as_array().unwrap();
    json!({"case":"retirement_ring", "notes":notes.len(),
        "first_retained":notes.iter().any(|x| x["id"]==json!(first.unwrap())),
        "latest_retained":notes.iter().any(|x| x["id"]==json!(last.unwrap()))})
}

async fn stale_materialized_metadata() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut cp, mut item) = seeded(SimpleContextConfig {
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .await;
    item["kind"] = serde_json::to_value(ContextKind::ToolObservation).unwrap();
    item["source"] = json!("fs.read");
    item["file_path"] = json!("src/a.rs");
    item["file_revision"] = json!("rev-1");
    item["content"] = json!("1 | fn a() {}\n");
    cp["eviction_buffer"] = json!([item.clone()]);
    engine.restore(cp).await.unwrap();
    assert_eq!(engine.gc().await.unwrap().externalized, 1);
    let mut cp = engine.checkpoint().await.unwrap();
    cp["external"][0]["retention"] = serde_json::to_value(ContextRetention::Pinned).unwrap();
    cp["external"][0]["scope"] = serde_json::to_value(ContextScope::Session).unwrap();
    engine.restore(cp).await.unwrap();
    let id: ContextItemId = serde_json::from_value(item["id"].clone()).unwrap();
    let fetched = engine.fetch_external(id).await.unwrap().unwrap();
    let foreground = engine.materialize(query("src/a.rs")).await.unwrap();
    let required = engine
        .materialize(ContextQuery {
            current_input: "inspect pinned context".into(),
            budget_tokens: 10000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    json!({"case":"stored_materialization_metadata",
        "fetch_retention":fetched.retention, "fetch_scope":fetched.scope,
        "foreground":foreground.foreground.iter().map(|x|json!({"scope":x.scope,"retention":x.retention})).collect::<Vec<_>>(),
        "required":required.items.iter().filter(|x|x.item_id==id).map(|x|json!({"scope":x.scope,"retention":x.retention})).collect::<Vec<_>>()})
}

async fn retired_scope_recalled_into_checkpoint() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut cp, mut item) = seeded(SimpleContextConfig {
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        scope_retire_target: 2,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .await;
    let scope_id: ScopeId = serde_json::from_value(cp["active_scope_id"].clone()).unwrap();
    item["scope_id"] = json!(scope_id);
    item["content"] = json!("old task evidence");
    let id: ContextItemId = serde_json::from_value(item["id"].clone()).unwrap();
    cp["eviction_buffer"] = json!([item]);
    engine.restore(cp).await.unwrap();
    assert_eq!(engine.gc().await.unwrap().externalized, 1);
    engine.close_scope(scope_id).await.unwrap();
    engine.gc().await.unwrap();
    let retired = engine.checkpoint().await.unwrap();
    assert!(
        retired["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["id"] != json!(scope_id))
    );
    assert!(retired["external"][0]["scope_id"].is_null());
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![AnchorRootClaim {
                    item_ref: id.to_string(),
                    strength: AnchorRootStrength::ResidentRequired,
                    source_field_id: "audit".into(),
                    ..Default::default()
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    let recalled = engine.checkpoint().await.unwrap();
    let restore_error = engine.restore(recalled).await.err().map(|e| e.to_string());
    json!({"case":"retired_scope_recall", "reactivated":report.reactivated,
        "restore_error":restore_error})
}

async fn store_outage_growth() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        max_pending_externalize_items: 3,
        gc_externalize_batch: 2,
        context_store_dir: Some(blocker.join("store")),
        ..Default::default()
    });
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "continue while store unavailable"),
        })
        .await
        .unwrap();
    let mut samples = Vec::new();
    for round in 0..40 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("round {round}: keep working while store unavailable"),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                scope_id: None,
                output: ToolOutput {
                    call_id: round.to_string(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("observation {round}"),
                    artifact_ref: None,
                    metadata: json!({}),
                },
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        let report = engine.gc().await.unwrap();
        if [5, 19, 39].contains(&round) {
            let cp = engine.checkpoint().await.unwrap();
            samples.push(json!({"rounds":round+1,"pending":cp["pending_externalize_retry"].as_array().unwrap().len(),
                "warm":cp["eviction_buffer"].as_array().unwrap().len(),"backpressure":report.externalize_backpressure}));
        }
    }
    json!({"case":"store_outage_growth", "samples":samples})
}

async fn service_recovery_roots() -> Value {
    use context_contextcore::{ContextServiceAdapter, ContextServiceConfig};
    let dir = tempfile::tempdir().unwrap();
    let (engine, mut cp, item) = seeded(SimpleContextConfig {
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .await;
    let id: ContextItemId = serde_json::from_value(item["id"].clone()).unwrap();
    cp["eviction_buffer"] = json!([item]);
    engine.restore(cp).await.unwrap();
    assert_eq!(engine.gc().await.unwrap().externalized, 1);
    let retained_checkpoint = engine.checkpoint().await.unwrap();
    let direct_roots = engine.checkpoint_recovery_item_ids(&retained_checkpoint);
    let program = std::env::current_dir()
        .unwrap()
        .join("target/debug/agent-context-service.exe");
    let adapter = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(program.to_string_lossy().into_owned()),
        store_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .await
    .unwrap();
    adapter.restore(retained_checkpoint.clone()).await.unwrap();
    adapter
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: id,
                reason: "use this retained evidence".into(),
            },
        })
        .await
        .unwrap();
    let adapter_roots = adapter.checkpoint_recovery_item_ids(&retained_checkpoint);
    let report = adapter
        .reconcile_store_protecting(&adapter_roots, true)
        .await
        .unwrap();
    let blob_survived = dir.path().join(format!("{id}.json")).exists();
    adapter.restore(retained_checkpoint).await.unwrap();
    let old_body_readable = adapter.fetch_external(id).await.unwrap().is_some();
    adapter.shutdown().await;
    json!({"case":"service_recovery_roots", "direct_roots":direct_roots.len(),
        "adapter_roots":adapter_roots.len(), "blob_survived":blob_survived,
        "retained_checkpoint_body_readable":old_body_readable,"reconcile":report})
}

async fn compatible_decisions() -> Value {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(
                TaskId::new(),
                "update logging without changing the timeout",
            ),
        })
        .await
        .unwrap();
    let old = "use AuthService.rs with a 5-second timeout";
    let new = "replace timeout logging in AuthService.rs with structured events";
    for content in [old, new] {
        engine
            .ingest(ContextIngress::UserMessage {
                content: content.into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }
    let cp = engine.checkpoint().await.unwrap();
    let row = cp["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["content"] == old)
        .unwrap();
    json!({"case":"compatible_decisions", "old":old,"new":new,"old_semantic":row["semantic"]})
}

struct FailingCompactor(std::sync::Mutex<Vec<CompactionRequest>>);

#[async_trait::async_trait]
impl BoundedCompactor for FailingCompactor {
    async fn compact(&self, request: CompactionRequest) -> AgentResult<CompactionOutput> {
        self.0.lock().unwrap().push(request);
        Err(AgentError::Model("local audit compactor fails".into()))
    }
}

async fn rolling_same_request_backoff() -> Value {
    use context_baselines::{RollingConfig, RollingSummaryEngine};
    let compactor = std::sync::Arc::new(FailingCompactor(std::sync::Mutex::new(Vec::new())));
    let engine = RollingSummaryEngine::with_config(RollingConfig {
        summary_threshold_tokens: 1000,
        keep_most_recent_tokens: 500,
        compact_failure_backoff_maintains: 4,
        ..Default::default()
    })
    .with_compactor(compactor.clone());
    for content in ["A".repeat(4000), "R".repeat(2000)] {
        engine
            .ingest(ContextIngress::UserMessage { content })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "Z".repeat(2000),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let calls = compactor.0.lock().unwrap();
    json!({"case":"rolling_same_request_backoff", "calls":calls.len(),
        "same_source":calls.len()==2 && calls[0].source==calls[1].source,
        "same_folded_items":calls.len()==2 && calls[0].folded_items==calls[1].folded_items,
        "source_chars":calls.first().map(|x|x.source.chars().count())})
}

#[tokio::main]
async fn main() {
    for result in [
        pending_foreground().await,
        warm_recall_budget().await,
        retirement_ring().await,
        stale_materialized_metadata().await,
        retired_scope_recalled_into_checkpoint().await,
        store_outage_growth().await,
        service_recovery_roots().await,
        compatible_decisions().await,
        rolling_same_request_backoff().await,
    ] {
        println!("{}", result);
    }
}

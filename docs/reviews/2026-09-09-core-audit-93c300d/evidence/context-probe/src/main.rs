use agent_contracts::*;
use context_baselines::{RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingCompactor(Mutex<Vec<String>>);
#[async_trait::async_trait]
impl BoundedCompactor for RecordingCompactor {
    async fn compact(&self, request: CompactionRequest) -> AgentResult<CompactionOutput> {
        self.0.lock().unwrap().push(request.source);
        Ok(CompactionOutput {
            text: "summary of the supplied prefix".into(),
            ..Default::default()
        })
    }
}

async fn rolling_source_coverage() {
    let compactor = Arc::new(RecordingCompactor::default());
    let engine = RollingSummaryEngine::with_config(RollingConfig::default())
        .with_compactor(compactor.clone());
    let marker = "UNIQUE_REQUIRED_CONSTRAINT_AT_THE_END";
    engine
        .ingest(ContextIngress::UserMessage {
            content: format!("{} {marker}", "a".repeat(5000)),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::AssistantMessage {
            content: "b".repeat(16000),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "c".repeat(16000),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let checkpoint = engine.checkpoint().await.unwrap();
    let inputs = compactor.0.lock().unwrap();
    println!(
        "{}",
        json!({"case":"rolling_source_coverage","default_config":true,"folded_records":report.archived,"compactor_calls":inputs.len(),"source_characters":inputs.iter().map(|s|s.chars().count()).collect::<Vec<_>>(),"constraint_sent_to_compactor":inputs.iter().any(|s|s.contains(marker)),"constraint_retained_in_checkpoint":checkpoint.to_string().contains(marker)})
    );
    assert_eq!(report.archived, 1);
    assert!(!inputs.iter().any(|s| s.contains(marker)));
    assert!(!checkpoint.to_string().contains(marker));
}

async fn pending_owner_storage_roots() {
    let directory = tempfile::tempdir().unwrap();
    let seed = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(directory.path().into()),
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        ..Default::default()
    });
    seed.ingest(ContextIngress::Pin {
        content: "Original evidence body".into(),
        kind: ContextKind::Note,
    })
    .await
    .unwrap();
    let mut snapshot = seed.checkpoint().await.unwrap();
    let mut source: ContextItem = serde_json::from_value(snapshot["items"][0].clone()).unwrap();
    source.retention = ContextRetention::Working;
    source.scope = ContextScope::Session;
    source.semantic = SemanticState::Tombstoned;
    source.attention = AttentionState::Archived;
    source.entities.clear();
    source.created_tick = 0;
    source.last_access_tick = 0;
    snapshot["items"] = json!([source]);
    seed.restore(snapshot).await.unwrap();
    assert_eq!(seed.gc().await.unwrap().externalized, 1);
    let mut external_snapshot = seed.checkpoint().await.unwrap();
    external_snapshot["event_seq"] = json!(100);
    let mut owner = source.clone();
    owner.id = ContextItemId::new();
    owner.content = "Live derived fact awaiting externalization".into();
    owner.semantic = SemanticState::Live;
    owner.residency = ContextResidency::Warm;
    owner.dependencies = vec![DependencyEdge {
        target: source.id,
        kind: DependencyKind::DerivedFrom,
    }];
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(directory.path().into()),
        ..Default::default()
    });
    let mut control = external_snapshot.clone();
    control["items"] = json!([owner]);
    engine.restore(control).await.unwrap();
    let protected = engine.storage_gc().await.unwrap();
    assert_eq!(protected.deleted, 0);
    // Construct the serialized fault-window owner location, keeping its
    // content, identity and strong edge identical to the control.
    external_snapshot["items"] = json!([]);
    external_snapshot["pending_externalize_retry"] = json!([owner]);
    engine.restore(external_snapshot).await.unwrap();
    let failed = engine.storage_gc().await.unwrap();
    let source_exists = directory
        .path()
        .join(format!("{}.json", source.id))
        .exists();
    let pending = engine.checkpoint().await.unwrap()["pending_externalize_retry"]
        .as_array()
        .unwrap()
        .len();
    println!(
        "{}",
        json!({"case":"pending_owner_storage_roots","heap_owner_deleted":protected.deleted,"pending_owner_deleted":failed.deleted,"live_pending_owners":pending,"referenced_evidence_still_exists":source_exists})
    );
    assert_eq!(failed.deleted, 1);
    assert!(!source_exists);
    assert_eq!(pending, 1);
    let gc = engine.gc().await.unwrap();
    let after = engine.checkpoint().await.unwrap();
    println!(
        "{}",
        json!({"case":"pending_only_gc_retry","externalized":gc.externalized,"pending_remaining":after["pending_externalize_retry"].as_array().unwrap().len(),"catalog_rows":engine.inspect(20).await.unwrap().len()})
    );
    assert_eq!(gc.externalized, 0);
    assert_eq!(
        after["pending_externalize_retry"].as_array().unwrap().len(),
        1
    );
}

async fn reconcile_retained_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(directory.path().into()),
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        ..Default::default()
    });
    engine
        .ingest(ContextIngress::Pin {
            content: "Retained source for a supported older checkpoint".into(),
            kind: ContextKind::Note,
        })
        .await
        .unwrap();
    let mut working = engine.checkpoint().await.unwrap();
    let mut item: ContextItem = serde_json::from_value(working["items"][0].clone()).unwrap();
    item.retention = ContextRetention::Working;
    item.scope = ContextScope::Session;
    item.attention = AttentionState::Archived;
    item.residency = ContextResidency::Warm;
    item.entities.clear();
    working["items"] = json!([]);
    working["eviction_buffer"] = json!([item]);
    engine.restore(working).await.unwrap();
    assert_eq!(engine.gc().await.unwrap().externalized, 1);
    let checkpoint_a = engine.checkpoint().await.unwrap();
    assert!(engine.fetch_external(item.id).await.unwrap().is_some());
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: item.id,
                reason: "explicit later use".into(),
            },
        })
        .await
        .unwrap();
    let checkpoint_b = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint_b).await.unwrap();
    let reconcile = engine.reconcile_store().await.unwrap();
    engine.restore(checkpoint_a).await.unwrap();
    let restored_body = engine.fetch_external(item.id).await.unwrap();
    println!(
        "{}",
        json!({"case":"retained_checkpoint_after_reconcile","deleted_as_stale":reconcile.deleted_stale,"older_checkpoint_body_available":restored_body.is_some()})
    );
    assert_eq!(reconcile.deleted_stale, 1);
    assert!(restored_body.is_none());
}

async fn disjoint_file_body_coverage() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "inspect src/a.rs"),
        })
        .await
        .unwrap();
    let marker = "UNIQUE_FIRST_WINDOW_CONSTRAINT";
    engine.ingest(ContextIngress::ToolObservation {
        facts: None,
        scope_id: None,
        output: ToolOutput {
            call_id: "first-window".into(), tool_name: "fs.read".into(), ok: true,
            summary: "read lines 1-100".into(),
            model_content: std::iter::once(marker.to_string()).chain((2..=100).map(|i|format!("line {i}"))).collect::<Vec<_>>().join("\n"),
            artifact_ref: None,
            metadata: json!({"path":"src/a.rs", "revision":"rev-1", "start_line":1, "end_line":100, "covers_file":false}),
        },
    }).await.unwrap();
    let control = engine
        .materialize(ContextQuery {
            current_input: "inspect src/a.rs".into(),
            budget_tokens: 10000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    // Exact hint shape generated by Runtime for a same-revision current
    // fs.read of lines 101-200. Runtime drops the range when generating it.
    let with_disjoint_hint = engine
        .materialize(ContextQuery {
            current_input: "inspect src/a.rs".into(),
            budget_tokens: 10000,
            hints: ContextHints {
                visible_body_identities: vec!["src/a.rs@rev-1".into()],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let control_visible = control
        .items
        .iter()
        .chain(control.foreground.iter())
        .any(|v| v.content.contains(marker));
    let hinted_visible = with_disjoint_hint
        .items
        .iter()
        .chain(with_disjoint_hint.foreground.iter())
        .any(|v| v.content.contains(marker));
    println!(
        "{}",
        json!({"case":"disjoint_file_body_coverage", "control_body_visible":control_visible,"body_visible_with_same_revision_other_window_hint":hinted_visible,"hinted_bodies":with_disjoint_hint.items.iter().map(|v|&v.content).collect::<Vec<_>>() })
    );
    assert!(control_visible);
    assert!(!hinted_visible);
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("range") {
        disjoint_file_body_coverage().await;
        return;
    }
    rolling_source_coverage().await;
    pending_owner_storage_roots().await;
    reconcile_retained_checkpoint().await;
    disjoint_file_body_coverage().await;
}

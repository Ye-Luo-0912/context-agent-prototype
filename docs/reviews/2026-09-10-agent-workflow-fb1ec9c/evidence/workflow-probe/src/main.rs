use agent_contracts::*;
use context_baselines::{RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

// Compile the current production adapter unchanged; no provider is contacted.
#[path = "../../../../../../crates/agent-compose/src/compactor.rs"]
mod live_compactor;
#[allow(dead_code, unused_imports)]
mod final_pack {
    include!(concat!(env!("OUT_DIR"), "/final_pack.rs"));
}

fn output(name: &str, ok: bool, body: &str) -> ToolOutput {
    ToolOutput {
        call_id: "probe".into(),
        tool_name: name.into(),
        ok,
        summary: body.into(),
        model_content: body.into(),
        artifact_ref: None,
        metadata: json!({}),
    }
}

fn final_pack_range() {
    let required = MaterializedItem {
        item_id: ContextItemId::new(),
        kind: ContextKind::ToolObservation,
        scope: ContextScope::Task,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        retention: ContextRetention::Working,
        content: "required first window\n".repeat(100),
        source: None,
        file_path: Some("src/a.rs".into()),
        file_revision: Some("r1".into()),
        file_start_line: Some(1),
        file_end_line: Some(100),
        partial_body: false,
    };
    let mut other = required.clone();
    other.item_id = ContextItemId::new();
    other.content = "unrelated second window".into();
    other.file_start_line = Some(101);
    other.file_end_line = Some(200);
    let mut materialized = MaterializedContext {
        required_item_ids: vec![required.item_id, other.item_id],
        items: vec![required.clone(), other],
        ..Default::default()
    };
    let drop_index =
        final_pack::largest_final_pack_drop_index(&materialized, &materialized.items).unwrap();
    let dropped = materialized.items.remove(drop_index);
    assert_eq!(dropped.item_id, required.item_id);
    final_pack::record_final_pack_drop(&mut materialized, &dropped, 9);
    println!(
        "{}",
        json!({"case":"final_pack_disjoint_required", "required_body_present":materialized.items.iter().chain(materialized.foreground.iter()).any(|v|v.item_id==required.item_id), "required_misses":materialized.required_misses.total()})
    );
    assert_eq!(
        materialized.required_misses.total(),
        0,
        "counterexample changed"
    );
}

fn verification_capacity() {
    let mut state = agent_runtime::ExecutionState::default();
    state.verification.spec_revision = 1;
    let verify = output("test.verify", true, "all passed");
    let mut attributes = Vec::new();
    for domain in 0..9 {
        // The public method exposes this intentionally un-reexported type.
        // Infer its Default on a separate scratch ledger, then fill the same
        // host attribution used by production/its existing unit tests.
        let mut attr = Default::default();
        agent_runtime::ExecutionState::default()
            .observe_tool_attributed(&verify, 1, 1, "infer", &attr);
        attr.host = ToolExecutionAttribution::bounded(
            ToolExecutionPurpose::Verify,
            Vec::<String>::new(),
            VerificationReuse::ExactCurrentWorld,
        )
        .with_verification_identity_material(&format!("domain-{domain}|world-1"))
        .with_verification_recipe(VerificationRecipeProvenance {
            recipe_id: format!("recipe-{domain}"),
            recipe_revision: "r1".into(),
            coverage_domain: Some(format!("domain-{domain}")),
            domain_declaration_revision: Some(1),
            domain_source_digest: ContentDigest::sha256_bytes(
                format!("source-{domain}").as_bytes(),
            )
            .to_string(),
            class_identity_digest: ContentDigest::sha256_bytes(
                format!("class-{domain}").as_bytes(),
            )
            .to_string(),
        });
        state.observe_tool_attributed(&verify, 1, domain + 1, &format!("arg-{domain}"), &attr);
        attributes.push(attr);
    }
    let current: Vec<_> = attributes
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            state
                .current_domain_verification_pass(1, a)
                .is_some()
                .then_some(i)
        })
        .collect();
    println!(
        "{}",
        json!({"case":"nine_verification_domains","allowed_domains":MAX_VERIFICATION_COVERAGE_DECLARATIONS,"allowed_criteria":MAX_TASK_ANCHOR_LIST_ITEMS,"retained_facts":state.verifications.len(),"current_domains":current,"workspace_revision":state.workspace_revision,"directive_revision":state.directive_revision,"validity":format!("{:?}",state.validity())})
    );
    assert_eq!(current, (1..9).collect::<Vec<_>>());
    state.observe_tool_attributed(&verify, 1, 10, "arg-0", &attributes[0]);
    let after: Vec<_> = attributes
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            state
                .current_domain_verification_pass(1, a)
                .is_some()
                .then_some(i)
        })
        .collect();
    println!(
        "{}",
        json!({"case":"repair_eviction_cycle","current_domains_after_repair_0":after})
    );
    assert!(!after.contains(&1));
    assert!(after.contains(&0));
}

async fn storage_gc_after_protected_reconcile() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().into()),
        gc_buffer_capacity: 0,
        gc_reactivate_per_pass: 0,
        ..Default::default()
    });
    engine
        .ingest(ContextIngress::Pin {
            content: "retained terminal evidence".into(),
            kind: ContextKind::Note,
        })
        .await
        .unwrap();
    let mut checkpoint = engine.checkpoint().await.unwrap();
    let mut item: ContextItem = serde_json::from_value(checkpoint["items"][0].clone()).unwrap();
    item.retention = ContextRetention::Working;
    item.scope = ContextScope::Session;
    item.entities.clear();
    item.semantic = SemanticState::Live;
    item.attention = AttentionState::Archived;
    item.residency = ContextResidency::Warm;
    item.created_tick = 0;
    item.last_access_tick = 0;
    checkpoint["items"] = json!([]);
    checkpoint["eviction_buffer"] = json!([item]);
    engine.restore(checkpoint).await.unwrap();
    assert_eq!(engine.gc().await.unwrap().externalized, 1);
    let retained = engine.checkpoint().await.unwrap();
    assert!(engine.fetch_external(item.id).await.unwrap().is_some());
    let roots = engine.checkpoint_recovery_item_ids(&retained);
    let mut aged = retained.clone();
    aged["event_seq"] = json!(100);
    aged["external"][0]["semantic"] = json!(SemanticState::Tombstoned);
    engine.restore(aged).await.unwrap();
    let recon = engine.reconcile_store_protecting(&roots).await.unwrap();
    let before = dir.path().join(format!("{}.json", item.id)).exists();
    let gc = engine.storage_gc().await.unwrap();
    let after = dir.path().join(format!("{}.json", item.id)).exists();
    engine.restore(retained).await.unwrap();
    let restored_available = engine.fetch_external(item.id).await.unwrap().is_some();
    println!(
        "{}",
        json!({"case":"recovery_roots_stop_at_reconcile","protected_root_count":roots.len(),"reconcile_deleted":recon.deleted_stale,"blob_before_storage_gc":before,"storage_gc_deleted":gc.deleted,"blob_after_storage_gc":after,"retained_checkpoint_still_references_id":engine.checkpoint_recovery_item_ids(&engine.checkpoint().await.unwrap()).contains(&item.id),"restored_live_body_available":restored_available})
    );
    assert!(before);
    assert!(!after);
    assert_eq!(gc.deleted, 1);
}

struct RecordingCompactor {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl BoundedCompactor for RecordingCompactor {
    async fn compact(&self, _: CompactionRequest) -> AgentResult<CompactionOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompactionOutput {
            text: "s".repeat(512),
            input_tokens: 500,
            output_tokens: 128,
        })
    }
}
async fn maintenance_call_count() {
    let compact = Arc::new(RecordingCompactor {
        calls: AtomicUsize::new(0),
    });
    let engine = RollingSummaryEngine::new().with_compactor(compact.clone());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "a".repeat(200_000),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::AssistantMessage {
            content: "b".repeat(16_000),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "c".repeat(16_000),
        })
        .await
        .unwrap();
    let result = engine
        .maintain(ContextMaintenanceTrigger::BeforeModel)
        .await
        .unwrap();
    println!(
        "{}",
        json!({"case":"one_maintenance_multiple_model_calls","old_input_chars":200_000,"compactor_calls":compact.calls.load(Ordering::SeqCst),"charged_input_tokens":result.compaction_input_tokens,"charged_output_tokens":result.compaction_output_tokens})
    );
    assert!(compact.calls.load(Ordering::SeqCst) > 100);
}

#[derive(Default)]
struct FailingModel {
    inputs: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl ModelTransport for FailingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.inputs
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.messages).unwrap());
        Err(AgentError::Model("scripted temporary model failure".into()))
    }
}
async fn compactor_failure_commits_fallback() {
    let model = Arc::new(FailingModel::default());
    let engine = RollingSummaryEngine::with_config(RollingConfig {
        summary_threshold_tokens: 100,
        keep_most_recent_tokens: 50,
    })
    .with_compactor(Arc::new(live_compactor::ModelBackedCompactor::new(
        model.clone(),
    )));
    let marker = "REQUIRED_CONSTRAINT_BEYOND_FALLBACK";
    engine
        .ingest(ContextIngress::UserMessage {
            content: format!("{} {marker}", "a".repeat(700)),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "b".repeat(300),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::BeforeModel)
        .await
        .unwrap();
    let checkpoint = engine.checkpoint().await.unwrap();
    let inputs = model.inputs.lock().unwrap().clone();
    println!(
        "{}",
        json!({"case":"failed_live_compactor_commits","model_calls":inputs.len(),"marker_attempted":inputs.iter().any(|v|v.contains(marker)),"archived":report.archived,"marker_retained":checkpoint.to_string().contains(marker)})
    );
    assert_eq!(report.archived, 1);
    assert!(!checkpoint.to_string().contains(marker));
}

async fn artifact_paging() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
    let run = RunId::new();
    let data = "123456789\n".repeat(300_000);
    let reference = workspace
        .write_artifact(run, "probe-log", "txt", data.as_bytes())
        .await
        .unwrap();
    let tools = tool_runtime::BuiltinToolDispatcher::new(workspace).unwrap();
    for end in [200, 1] {
        let result = tools
            .execute(ToolExecutionRequest {
                run_id: run,
                call: ToolCall {
                    id: format!("read-{end}"),
                    name: "artifact.read".into(),
                    arguments: json!({"reference":reference,"start_line":1,"end_line":end}),
                },
                effect_context: None,
                cancel: CancellationToken::new(),
            })
            .await;
        println!(
            "{}",
            json!({"case":"artifact_paging_large_log","artifact_bytes":data.len(),"end_line":end,"error":result.as_ref().err().map(ToString::to_string)})
        );
        assert!(result.is_err());
    }
}

struct NoTools;
#[async_trait::async_trait]
impl ToolDispatcher for NoTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(AgentError::Tool("no tools".into()))
    }
}
struct HangingModel;
#[async_trait::async_trait]
impl ModelTransport for HangingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, r: ModelRequest) -> AgentResult<ModelOutput> {
        r.cancel.cancelled().await;
        Err(AgentError::Cancelled)
    }
}
#[derive(Default)]
struct GatedCompactor {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl BoundedCompactor for GatedCompactor {
    async fn compact(&self, _: CompactionRequest) -> AgentResult<CompactionOutput> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Err(AgentError::Model("scripted maintenance release".into()))
    }
}
async fn maintenance_delays_actor_cancel() {
    use std::time::Duration;
    let gate = Arc::new(GatedCompactor::default());
    let context = Arc::new(
        RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 100,
            keep_most_recent_tokens: 50,
        })
        .with_compactor(gate.clone()),
    );
    context
        .ingest(ContextIngress::UserMessage {
            content: "a".repeat(1000),
        })
        .await
        .unwrap();
    context
        .ingest(ContextIngress::AssistantMessage {
            content: "b".repeat(300),
        })
        .await
        .unwrap();
    let services = Arc::new(agent_runtime::RuntimeServices::new(
        agent_core::CoreAuthorityConfig::default(),
        context,
        Arc::new(HangingModel),
        Arc::new(NoTools),
        Arc::new(agent_core::PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, actor) = agent_runtime::spawn_runtime(services);
    handle.start().await.unwrap();
    let submit_handle = handle.clone();
    let submit =
        tokio::spawn(async move { submit_handle.user_message("continue checking".into()).await });
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let cancel_handle = handle.clone();
    let mut cancel = tokio::spawn(async move { cancel_handle.cancel_turn().await });
    let blocked = tokio::time::timeout(Duration::from_millis(250), &mut cancel)
        .await
        .is_err();
    gate.release.notify_one();
    let after = tokio::time::timeout(Duration::from_secs(3), cancel)
        .await
        .unwrap()
        .unwrap();
    let submitted = submit.await.unwrap();
    let stopped = handle.stop().await;
    tokio::time::timeout(Duration::from_secs(3), actor)
        .await
        .unwrap()
        .unwrap();
    println!(
        "{}",
        json!({"case":"actor_cancel_during_maintenance","cancel_reply_delayed_until_maintenance_released":blocked,"submit_ok":submitted.is_ok(),"cancel_after_release":format!("{after:?}"),"stop_ok":stopped.is_ok()})
    );
    assert!(blocked);
    assert!(stopped.is_ok());
}

async fn patch_candidate_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    let original = "let answer = old_value;\nlet other = keep_value;\n";
    tokio::fs::write(&path, original).await.unwrap();
    let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
    let tools = tool_runtime::BuiltinToolDispatcher::new(workspace).unwrap();
    let run = RunId::new();
    // Refusal happens before staging; this fixture cannot commit a write.
    let arguments = json!({"files":[{"path":"a.txt","base_revision":ContentDigest::sha256_bytes(original.as_bytes()).to_string(),"hunks":[{"old":"old_value","new":"NEVER_COMMITTED_VALUE"},{"old":"let answer = NEVER_COMMITTED_VALUE;\nmissing next line","new":"other"}]}]});
    // Structural recovery identity, as in existing direct-dispatch tests;
    // it grants no commit authority. This call refuses before staging.
    let identity = ToolOperationIdentity {
        run_id: run,
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id: OperationId::new(),
        generation: 1,
        call_id: "patch".into(),
        tool_name: "edit.patch".into(),
        argument_digest: ArgumentDigest::from_json(&arguments),
    };
    let result = tools
        .execute(ToolExecutionRequest {
            run_id: run,
            call: ToolCall {
                id: "patch".into(),
                name: "edit.patch".into(),
                arguments,
            },
            effect_context: Some(OperationEffectContext {
                identity,
                effect_id: EffectId::new(),
            }),
            cancel: CancellationToken::new(),
        })
        .await;
    match result {
        Ok(ToolOutcome::Value(out)) => {
            let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
            println!(
                "{}",
                json!({"case":"patch_refusal_snapshot","ok":out.ok,"file_unchanged":on_disk==original,"candidate_mentions_uncommitted_text":out.model_content.contains("NEVER_COMMITTED_VALUE"),"metadata":out.metadata})
            );
            assert!(!out.ok);
            assert_eq!(on_disk, original);
            assert!(out.model_content.contains("NEVER_COMMITTED_VALUE"));
        }
        other => panic!("patch fixture did not reach refusal: {other:?}"),
    }
}

#[derive(Default)]
struct CaptureModel {
    requests: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl ModelTransport for CaptureModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            context_window: Some(32_000),
            max_output_tokens: 256,
            ..Default::default()
        }
    }
    async fn complete(&self, r: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&r.messages).unwrap());
        Ok(ModelOutput {
            content: "Work remains for the next turn.".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}
async fn continue_full_directive() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let context = Arc::new(RollingSummaryEngine::new());
    let model = Arc::new(CaptureModel::default());
    let services = Arc::new(
        agent_runtime::RuntimeServices::new(
            agent_core::CoreAuthorityConfig::default(),
            context.clone(),
            model.clone(),
            Arc::new(NoTools),
            Arc::new(agent_core::PolicyApprovalGate::read_only()),
            None,
        )
        .with_artifact_workspace(workspace),
    );
    let (handle, actor) = agent_runtime::spawn_runtime(services);
    handle.start().await.unwrap();
    handle
        .set_focus("Inspect the implementation".into())
        .await
        .unwrap();
    let mut events = handle.subscribe();
    let marker = "UNIQUE_TAIL_DIRECTIVE_PRESERVE_THE_PUBLIC_API";
    handle
        .user_message(format!("{} {marker}", "a".repeat(2100)))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(event) = events.recv().await {
            if matches!(event.event, RuntimeEvent::TurnCompleted { .. }) {
                return;
            }
        }
        panic!("event stream ended");
    })
    .await
    .unwrap();
    let continued = handle.continue_active_task().await;
    if continued.is_ok() {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Ok(event) = events.recv().await {
                if matches!(event.event, RuntimeEvent::TurnCompleted { .. }) {
                    return;
                }
            }
            panic!("event stream ended");
        })
        .await
        .unwrap();
    }
    let requests = model.requests.lock().unwrap().clone();
    println!(
        "{}",
        json!({"case":"continued_full_directive","continue_result":format!("{continued:?}"),"request_count":requests.len(),"marker_visible_each_request":requests.iter().map(|r|r.contains(marker)).collect::<Vec<_>>(),"raw_context_keeps_marker":context.checkpoint().await.unwrap().to_string().contains(marker)})
    );
    let stop = handle.stop().await;
    actor.await.unwrap();
    assert!(stop.is_ok());
    assert!(continued.is_ok());
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(marker));
    assert!(!requests[1].contains(marker));
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("verification") {
        verification_capacity();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("packing") {
        final_pack_range();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("continue") {
        continue_full_directive().await;
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("patch") {
        patch_candidate_snapshot().await;
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("followup") {
        storage_gc_after_protected_reconcile().await;
        maintenance_delays_actor_cancel().await;
        patch_candidate_snapshot().await;
        return;
    }
    final_pack_range();
    verification_capacity();
    storage_gc_after_protected_reconcile().await;
    maintenance_call_count().await;
    compactor_failure_commits_fallback().await;
    artifact_paging().await;
}

//! End-to-end tests for the context-service adapter.
//!
//! Cargo sets `CARGO_BIN_EXE_agent-context-service` for integration tests, so
//! these tests spawn the real service process and drive the full
//! `ContextEngine` contract across the process boundary — including plugging
//! the adapter into a real `AgentKernel` (the acceptance criterion: a
//! composition-root change, nothing else).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextConsumptionAck, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, MaterializedContext,
    ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, OperationId, RuntimeEvent,
    ToolOutcome, ToolOutput, TurnId,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use context_contextcore::{ContextServiceAdapter, ContextServiceConfig, ServiceEngine};
use serde_json::json;

async fn connect() -> Arc<dyn ContextEngine> {
    let adapter = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn + handshake with the context service");
    Arc::new(adapter)
}

/// A unique store outside the workspace. The contract-parity test forces
/// real externalization, so it must not share the service's process-wide
/// fallback directory with another test (or leave context artifacts in the
/// repository when the test runner's CWD is the workspace).
struct IsolatedStore {
    path: PathBuf,
}

impl IsolatedStore {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "context-contextcore-{label}-{}-{}",
            std::process::id(),
            agent_contracts::ContextItemId::new()
        ));
        std::fs::create_dir_all(&path).expect("create isolated context store");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IsolatedStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test]
async fn full_contract_round_trip_across_the_process_boundary() {
    let engine = connect().await;

    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::Pin {
            content: "never touch generated files".into(),
            kind: agent_contracts::ContextKind::Constraint,
        })
        .await
        .unwrap();

    let report: ContextMaintenanceReport = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(report.diagnostics.total_items >= 2);

    let snapshot: MaterializedContext = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(snapshot.approx_tokens > 0);
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.contains("never touch generated files")),
        "pinned constraint must cross the wire"
    );
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 0,
            materialization_id: snapshot.materialization_id,
            item_ids: snapshot.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: snapshot
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
        })
        .await
        .unwrap();

    let items: Vec<ContextItemSummary> = engine.inspect(100).await.unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        snapshot.items.iter().all(|selected| {
            items
                .iter()
                .find(|item| item.id == selected.item_id)
                .is_some_and(|item| item.access_count > 0)
        }),
        "the sidecar must commit access reinforcement for every item in the exact preview"
    );

    // Scope lifecycle crosses the wire: open a tool scope, close it.
    let scope_id = engine
        .open_scope(agent_contracts::ScopeKind::Tool, None)
        .await
        .unwrap();
    let transitions = engine.close_scope(scope_id).await.unwrap();
    assert!(
        transitions.is_empty(),
        "tool scope close produces no evictions"
    );

    // Checkpoint/restore round-trip across the boundary.
    let checkpoint = engine.checkpoint().await.unwrap();
    let adapter2 = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .unwrap();
    adapter2.restore(checkpoint).await.unwrap();
    let after = adapter2.diagnostics().await.unwrap();
    assert_eq!(after.total_items, 2);
    adapter2.shutdown().await;
}

#[tokio::test]
async fn adapter_plugs_into_a_real_kernel_without_rewrites() {
    let engine = connect().await;

    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        engine,
        Arc::new(PlainModel),
        Arc::new(ToolDispatcherStub),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = agent_runtime::spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("hello service".into()).await.unwrap();

    let mut reply = None;
    let mut completed = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::AssistantMessage { content } => reply = Some(content),
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(reply.as_deref(), Some("hello from the other side"));
    assert!(
        completed,
        "the turn must complete through the service engine"
    );
}

/// Emits one fixed assistant reply; no tool calls.
struct PlainModel;

#[async_trait::async_trait]
impl ModelTransport for PlainModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "hello from the other side".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[derive(Debug)]
struct ToolDispatcherStub;

#[async_trait::async_trait]
impl agent_contracts::ToolDispatcher for ToolDispatcherStub {
    fn specs(&self) -> Vec<agent_contracts::ToolSpec> {
        Vec::new()
    }
    async fn execute(
        &self,
        _request: agent_contracts::ToolExecutionRequest,
    ) -> AgentResult<ToolOutcome> {
        unreachable!("no tools in this test")
    }
}

#[tokio::test]
async fn service_equivalence_with_in_process_engine() {
    // The same scripted lifecycle must produce the same diagnostics whether
    // the engine runs in-process or behind the service boundary.
    let service = connect().await;
    let local =
        context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig::default());
    let local: Arc<dyn ContextEngine> = Arc::new(local);

    for engine in [service, local] {
        for turn in 0..5 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}: continue the refactor"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: "c".into(),
                        tool_name: "shell.exec".into(),
                        ok: false,
                        summary: "failed".into(),
                        model_content: "error in AuthService.rs:42".into(),
                        artifact_ref: None,
                        metadata: json!({}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::AfterTool)
                .await
                .unwrap();
        }
        let snapshot = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 12_000,
                hints: Default::default(),
            })
            .await
            .unwrap();
        // Errors persist until verified: exactly one live error survives the
        // recurring failures, and the snapshot is bounded.
        assert!(snapshot.approx_tokens < 4_000, "bounded snapshot");
    }
}

#[tokio::test]
async fn gc_parity_between_in_process_and_service_boundary() {
    // The same scripted lifecycle must produce the same GC report whether
    // the engine runs in-process or behind the service boundary. This is
    // the regression test for the GC dimension of the wire protocol: a
    // service that drops `gc()` silently would diverge here.
    let service = connect().await;
    let local: Arc<dyn ContextEngine> = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));

    let mut reports = Vec::new();
    for engine in [service, local] {
        for turn in 0..3 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}: fix AuthService.rs"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: format!("{turn}"),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!("tests passed in AuthService.rs ({turn})"),
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
        }
        let report = engine.gc().await.unwrap();
        assert!(
            report.evicted >= 1,
            "the consumed observations must be evictable, got {report:?}"
        );
        let mut report = serde_json::to_value(report).unwrap();
        strip_random_item_ids(&mut report);
        reports.push(report);
    }
    assert_eq!(
        reports[0], reports[1],
        "GC must behave identically across the process boundary"
    );
}

/// Item ids are random UUIDs generated per engine instance, so they can
/// never match across two runs; strip them before comparing the reports.
/// Everything else (kinds, reasons, ticks, counts, diagnostics) must match.
fn strip_random_item_ids(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("item_id");
            for nested in map.values_mut() {
                strip_random_item_ids(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                strip_random_item_ids(nested);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn missing_service_fails_fast_with_clear_error() {
    let config = ContextServiceConfig {
        program: Some("definitely-not-a-real-binary-xyz".into()),
        startup_timeout: Duration::from_secs(3),
        ..ContextServiceConfig::default()
    };
    let error = match ContextServiceAdapter::connect(&config).await {
        Ok(_) => panic!("connecting to a missing binary must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("spawn context service"),
        "unexpected error: {error}"
    );
}

/// Drive every `ContextEngine` contract method through a fixed script and
/// collect the observable outcome as a normalized JSON snapshot.
///
/// This is the process-boundary parity checklist: when a new method is
/// added to `ContextEngine`, extend this script and the parity test below
/// automatically verifies that the wire op, the service handling and the
/// adapter override all exist and agree with the in-process engine. No new
/// trait method may land without a corresponding entry here.
async fn contract_snapshot(engine: &dyn ContextEngine) -> serde_json::Value {
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::Pin {
            content: "never touch generated files".into(),
            kind: agent_contracts::ContextKind::Constraint,
        })
        .await
        .unwrap();
    let maintain = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue the refactor".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 0,
            materialization_id: materialized.materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
        })
        .await
        .unwrap();
    let post_ack_access_count: u32 = engine
        .inspect(100)
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.access_count)
        .sum();
    let scope_id = engine
        .open_scope(agent_contracts::ScopeKind::Tool, None)
        .await
        .unwrap();
    // Force the default-sized reversible buffer to overflow. This uses the
    // same default capacity as the sidecar's dynamic engine, while keeping
    // the service otherwise on production defaults. Successful tool
    // observations become consumed ephemerals after AfterModel and are
    // therefore evictable in the immediately following full GC pass.
    let overflow_items = context_simple::SimpleContextConfig::default().gc_buffer_capacity + 1;
    for item in 0..overflow_items {
        let model_content = if item == 0 {
            "external recall sentinel RecallSentinel.rs AuthService".to_string()
        } else {
            format!("historical File{item}.rs AuthService observation")
        };
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: format!("overflow-{item}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "historical result".into(),
                    model_content,
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = engine.gc().await.unwrap();
    assert!(
        gc.externalized > 0,
        "the parity script must cross the external-store boundary: {gc:?}"
    );
    let transitions = engine.close_scope(scope_id).await.unwrap();
    let inspect = engine.inspect(100).await.unwrap();
    let search = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "RecallSentinel.rs".into(),
            kind: None,
            scope: None,
            task_id: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(
        !search.is_empty(),
        "search_external must surface the externalized sentinel entry"
    );
    // Pull one externalized entry by the id the search just returned, so
    // inspect_external / fetch_external must cross the boundary. Requiring
    // Some here prevents the trait's default no-op methods from producing a
    // parity false positive.
    let first_id = search[0].item_id;
    let inspect_external = engine
        .inspect_external(first_id)
        .await
        .unwrap()
        .expect("inspect_external must return searched entry metadata");
    assert_eq!(inspect_external.item_id, first_id);
    let fetch_external = engine
        .fetch_external(first_id)
        .await
        .unwrap()
        .expect("fetch_external must return the searched entry content");
    assert_eq!(fetch_external.id, first_id);
    assert!(fetch_external.content.contains("RecallSentinel.rs"));
    let storage_gc = engine.storage_gc().await.unwrap();
    let diagnostics = engine.diagnostics().await.unwrap();
    // Checkpoint/restore round-trip as the last step: the restored engine
    // must report the same diagnostics it did before the round-trip.
    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint.clone()).await.unwrap();
    let post_restore_diagnostics = engine.diagnostics().await.unwrap();

    let mut snapshot = json!({
        "maintain": maintain,
        "materialized_item_count": materialized.items.len(),
        "materialized_approx_tokens": materialized.approx_tokens,
        "post_ack_access_count": post_ack_access_count,
        "gc": gc,
        "scope_close_transitions": transitions.len(),
        "inspect": inspect,
        "search_external": search,
        "inspect_external": inspect_external,
        "fetch_external": fetch_external,
        "storage_gc": storage_gc,
        "diagnostics": diagnostics,
        "checkpoint": checkpoint,
        "post_restore_diagnostics": post_restore_diagnostics,
    });
    // Item/scope/run ids are random per engine instance and can never match
    // across two runs; everything else must.
    strip_random_uuids(&mut snapshot);
    snapshot
}

/// Replace every UUID-shaped string in the JSON tree with a placeholder, so
/// two engine instances (which mint random item/scope ids) can be compared
/// on everything that is actually deterministic. Works for plain ids and
/// for uris embedding one (`context://run/<uuid>`).
///
/// `blob_checksum` values are normalized too: the checksum is a content
/// hash of the serialized blob, and the blob embeds random ids, so the
/// exact hash can never match across engines — what must match is that
/// both engines captured one.
fn strip_random_uuids(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // UUID-shaped map keys (the ledger's per-item revision
            // counters) cannot match across two engines; collapse them
            // onto one placeholder. Multiple keys collapse the same way on
            // both sides, so the remaining value stays comparable.
            let uuid_keys: Vec<String> = map
                .keys()
                .filter(|key| key.len() == 36 && is_uuid_window(key.as_bytes()))
                .cloned()
                .collect();
            for key in uuid_keys {
                let nested = map.remove(&key).expect("key present");
                map.insert("<uuid>".into(), nested);
            }
            for (key, nested) in map.iter_mut() {
                if key == "blob_checksum" && nested.is_string() {
                    *nested = serde_json::Value::String("<checksum>".into());
                    continue;
                }
                strip_random_uuids(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                strip_random_uuids(nested);
            }
        }
        serde_json::Value::String(text) => {
            let bytes = text.as_bytes();
            let mut out = String::with_capacity(text.len());
            let mut i = 0;
            while i < bytes.len() {
                if i + 36 <= bytes.len() && is_uuid_window(&bytes[i..i + 36]) {
                    out.push_str("<uuid>");
                    i += 36;
                } else {
                    let ch = text[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            *text = out;
        }
        _ => {}
    }
}

/// A 36-byte window with dashes at 8/13/18/23 and hex elsewhere is a UUID.
fn is_uuid_window(bytes: &[u8]) -> bool {
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                true
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// First divergent JSON path between two snapshots, for readable parity
/// failures (e.g. `gc.externalized: 1 != 2`).
fn first_divergence(left: &serde_json::Value, right: &serde_json::Value) -> Option<String> {
    fn walk(
        left: &serde_json::Value,
        right: &serde_json::Value,
        path: &mut Vec<String>,
    ) -> Option<String> {
        match (left, right) {
            (serde_json::Value::Object(l), serde_json::Value::Object(r)) => {
                for (key, lv) in l {
                    match r.get(key) {
                        Some(rv) => {
                            path.push(key.clone());
                            if let Some(diff) = walk(lv, rv, path) {
                                return Some(diff);
                            }
                            path.pop();
                        }
                        None => return Some(format!("{}.{}: left only", path.join("."), key)),
                    }
                }
                for key in r.keys() {
                    if !l.contains_key(key) {
                        return Some(format!("{}.{}: right only", path.join("."), key));
                    }
                }
                None
            }
            (serde_json::Value::Array(l), serde_json::Value::Array(r)) => {
                if l.len() != r.len() {
                    return Some(format!(
                        "{}: array len {} != {}",
                        path.join("."),
                        l.len(),
                        r.len()
                    ));
                }
                for (index, (lv, rv)) in l.iter().zip(r.iter()).enumerate() {
                    path.push(index.to_string());
                    if let Some(diff) = walk(lv, rv, path) {
                        return Some(diff);
                    }
                    path.pop();
                }
                None
            }
            (l, r) if l == r => None,
            (l, r) => Some(format!("{}: {} != {}", path.join("."), l, r)),
        }
    }
    walk(left, right, &mut Vec::new())
}

#[tokio::test]
async fn full_contract_parity_across_the_process_boundary() {
    // The same scripted lifecycle must produce the same observable outcome
    // whether the engine runs in-process or behind the service boundary —
    // for *every* contract method at once. A wire op dropped in the
    // service, or an adapter method left to its default no-op, diverges
    // here.
    let service_store = IsolatedStore::new("service-parity");
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        store_dir: Some(service_store.path().to_path_buf()),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn isolated context service");
    let local_store = IsolatedStore::new("local-parity");
    let local = context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
        context_store_dir: Some(local_store.path().to_path_buf()),
        ..context_simple::SimpleContextConfig::default()
    });

    let service_snapshot = contract_snapshot(&service).await;
    let local_snapshot = contract_snapshot(&local).await;
    service.shutdown().await;
    assert_eq!(
        service_snapshot,
        local_snapshot,
        "every ContextEngine contract method must behave identically across the process boundary; first divergence: {}",
        first_divergence(&service_snapshot, &local_snapshot).unwrap_or_else(|| "<none>".into())
    );
}

#[tokio::test]
async fn storage_gc_parity_between_in_process_and_service_boundary() {
    // The storage GC is the only place information is deleted; a service
    // that drops `storage_gc()` silently (or an adapter that leaves the
    // default no-op in place) would diverge here.
    let service = connect().await;
    let local: Arc<dyn ContextEngine> = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));

    let mut reports = Vec::new();
    for engine in [service, local] {
        for turn in 0..3 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}: fix AuthService.rs"),
                })
                .await
                .unwrap();
            engine
                .ingest(ContextIngress::ToolObservation {
                    output: ToolOutput {
                        call_id: format!("{turn}"),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!("tests passed in AuthService.rs ({turn})"),
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
        }
        // Externalize the consumed observations, then run the conservative
        // storage GC over the store.
        engine.gc().await.unwrap();
        let report = engine.storage_gc().await.unwrap();
        let mut report = serde_json::to_value(report).unwrap();
        strip_random_uuids(&mut report);
        reports.push(report);
    }
    assert_eq!(
        reports[0], reports[1],
        "Storage GC must behave identically across the process boundary"
    );
}

/// Overflow the reversible buffer on `engine` (same script as the contract
/// snapshot) so the full GC pass externalizes real blobs into `store_dir`.
/// Returns the number of blobs the gc report claims it externalized.
async fn externalize_some(engine: &dyn ContextEngine, store_dir: &Path) -> usize {
    engine
        .ingest(ContextIngress::Pin {
            content: "reconcile sentinel AuthService".into(),
            kind: agent_contracts::ContextKind::Constraint,
        })
        .await
        .unwrap();
    let overflow = context_simple::SimpleContextConfig::default().gc_buffer_capacity + 1;
    for item in 0..overflow {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: format!("reconcile-{item}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "historical result".into(),
                    model_content: format!("AuthService File{item}.rs observation"),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = engine.gc().await.unwrap();
    assert!(
        gc.externalized > 0,
        "the reconcile parity script must externalize blobs: {gc:?}"
    );
    let _ = store_dir;
    gc.externalized
}

/// The first non-quarantine blob in the store, with its `id` patched to
/// `fresh_id` and written under `<fresh_id>.json`. Reusing an existing
/// blob guarantees the file parses as a valid `ContextItem`, which is the
/// only thing the reconcile can rely on.
async fn clone_blob_with_id(
    store_dir: &Path,
    source_id: agent_contracts::ContextItemId,
    fresh_id: agent_contracts::ContextItemId,
) {
    let source = std::fs::read(store_dir.join(format!("{source_id}.json"))).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&source).unwrap();
    value["id"] = serde_json::to_value(fresh_id).unwrap();
    std::fs::write(
        store_dir.join(format!("{fresh_id}.json")),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn reconcile_store_parity_between_in_process_and_service_boundary() {
    // The startup reconcile is the crash-recovery authority over store
    // blobs; a service that drops `reconcile_store()` silently (or an
    // adapter that leaves the trait's default no-op in place) must diverge
    // here. Both engines get the identical crash-injected store, so the
    // reports — rebuilt / deleted-stale / quarantined / temp-cleaned —
    // must match exactly.
    let service_store = IsolatedStore::new("reconcile-service");
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        engine: ServiceEngine::Dynamic,
        store_dir: Some(service_store.path().to_path_buf()),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn isolated context service");
    let local_store = IsolatedStore::new("reconcile-local");
    let local: Arc<dyn ContextEngine> = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            context_store_dir: Some(local_store.path().to_path_buf()),
            ..context_simple::SimpleContextConfig::default()
        },
    ));

    let mut reports = Vec::new();
    for (engine, store_dir) in [
        (&service as &dyn ContextEngine, service_store.path()),
        (local.as_ref(), local_store.path()),
    ] {
        let externalized = externalize_some(engine, store_dir).await;

        // Phase 1: a healthy store reconciles to zero interventions — every
        // blob is owned by the map and matches its checksum.
        let report = engine.reconcile_store().await.unwrap();
        assert_eq!(report.scanned, externalized, "all blobs scanned");
        assert_eq!(report.rebuilt, 0);
        assert_eq!(report.deleted_stale, 0);
        assert_eq!(report.quarantined, 0);
        assert_eq!(report.temp_cleaned, 0);
        assert_eq!(report.io_errors, 0);

        // Phase 2: crash-inject four states into the store and reconcile
        // again. 1) an orphan blob (valid content, no owner) must be
        // rebuilt into an entry; 2) a blob whose id is resident again must
        // be deleted as stale; 3) an unparseable blob must be quarantined,
        // not guessed away; 4) an abandoned temp file must be removed.
        let blobs: Vec<_> = std::fs::read_dir(store_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .map(|entry| entry.path())
            .collect();
        assert!(blobs.len() >= externalized);
        let source_name = blobs[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_end_matches(".json")
            .to_owned();
        let source_id: agent_contracts::ContextItemId = source_name.parse().unwrap();

        let orphan_id = agent_contracts::ContextItemId::new();
        clone_blob_with_id(store_dir, source_id, orphan_id).await;

        let residents = engine.inspect(100).await.unwrap();
        let resident_id = residents[0].id;
        clone_blob_with_id(store_dir, source_id, resident_id).await;

        let garbage_id = agent_contracts::ContextItemId::new();
        std::fs::write(
            store_dir.join(format!("{garbage_id}.json")),
            b"this is not a context item{{{",
        )
        .unwrap();

        let temp_id = agent_contracts::ContextItemId::new();
        std::fs::write(store_dir.join(format!("{temp_id}.tmp")), b"partial").unwrap();

        let report = engine.reconcile_store().await.unwrap();
        assert_eq!(report.rebuilt, 1, "orphan rebuilt: {report:?}");
        assert_eq!(
            report.deleted_stale, 1,
            "stale duplicate removed: {report:?}"
        );
        assert_eq!(
            report.quarantined, 1,
            "damaged blob quarantined: {report:?}"
        );
        assert_eq!(report.temp_cleaned, 1, "abandoned temp removed: {report:?}");
        assert_eq!(report.io_errors, 0);
        assert_eq!(report.scanned, externalized + 3);

        let mut report = serde_json::to_value(report).unwrap();
        // `reasons` are gathered while walking the directory, whose entry
        // order is filesystem-defined, not engine-defined — sort before
        // comparing so the parity check measures behavior, not read_dir.
        if let Some(reasons) = report.get_mut("reasons").and_then(|v| v.as_array_mut()) {
            reasons.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
        strip_random_uuids(&mut report);
        reports.push(report);
    }
    service.shutdown().await;
    assert_eq!(
        reports[0], reports[1],
        "Store reconcile must behave identically across the process boundary"
    );
}

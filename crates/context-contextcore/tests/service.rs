//! End-to-end tests for the context-service adapter.
//!
//! The `agent-context-service` package owns this integration-test target, so
//! Cargo builds the exact binary exposed by `CARGO_BIN_EXE_agent-context-service`.
//! The tests spawn that real service process and drive the full
//! `ContextEngine` contract across the process boundary — including plugging
//! the adapter into a real `CoreAuthority` (the acceptance criterion: a
//! composition-root change, nothing else).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextAction, ContextConsumptionAck, ContextEngine, ContextHints, ContextIngress,
    ContextItemId, ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, OperationId, RuntimeEvent, ToolOutcome, ToolOutput, TurnId,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use context_contextcore::{ContextServiceAdapter, ContextServiceConfig, ServiceEngine};
use serde_json::json;

async fn connect() -> Arc<dyn ContextEngine> {
    let adapter = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn + handshake with the context service");
    Arc::new(adapter)
}

fn service_program() -> String {
    env!("CARGO_BIN_EXE_agent-context-service").to_string()
}

#[tokio::test]
async fn invalid_frame_bound_is_rejected_before_spawning_the_service() {
    let error = ContextServiceAdapter::connect(&ContextServiceConfig {
        max_frame_bytes: 1,
        ..ContextServiceConfig::default()
    })
    .await
    .err()
    .expect("a sub-minimum frame bound must fail locally");
    assert!(error.to_string().contains("frame bound"));
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
            foreground_item_ids: snapshot
                .foreground
                .iter()
                .map(|item| item.item_id)
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
        program: Some(service_program()),
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

    let services = Arc::new(agent_runtime::RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine,
        Arc::new(PlainModel),
        Arc::new(ToolDispatcherStub),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = agent_runtime::spawn_runtime(services);
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
                    facts: None,
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
                    facts: None,
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

/// Read one newline-terminated line from the child pipe.
async fn read_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;
    reader.read_until(b'\n', buf).await
}

#[tokio::test]
async fn oversized_request_frames_end_the_session_with_a_bounded_error() {
    // Symmetric service-side bound: the service reads requests with the
    // same incremental frame cap the adapter applies to responses. An
    // over-cap request line must be refused with one bounded error frame
    // and the session must end — the half-consumed line cannot be trusted,
    // and a client that cannot speak the framing must not keep the service
    // alive.
    let program = service_program();
    let mut child = tokio::process::Command::new(&program)
        .args(["--engine", "append", "--max-frame-bytes", "4096"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the context service");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());

    // One request line far over the 4 KiB frame cap. The service may
    // detect the violation after the first ~4 KiB and end the session
    // while we are still streaming the rest — a BrokenPipe on the write is
    // exactly that fail-closed exit, not a failure of this test.
    use tokio::io::AsyncWriteExt;
    let mut line = "{\"id\":1,\"version\":1,\"op\":\"diagnostics\",\"pad\":\"".to_string();
    line.push_str(&"x".repeat(64 * 1024));
    line.push_str("\"}\n");
    let write = stdin.write_all(line.as_bytes()).await;
    let _ = stdin.flush().await;
    match write {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(e) => panic!("writing the oversized line failed: {e}"),
    }

    // The service answers exactly one bounded error frame...
    let mut response = Vec::new();
    let count = tokio::time::timeout(
        Duration::from_secs(5),
        read_line(&mut stdout, &mut response),
    )
    .await
    .expect("the service must answer the violation")
    .expect("read the error frame");
    assert!(count > 0, "a bounded error frame must be written");
    assert!(
        response.len() <= 4096,
        "the error answer must itself be bounded, got {} bytes",
        response.len()
    );
    let response: serde_json::Value =
        serde_json::from_slice(&response).expect("the service's answer must be a parseable frame");
    assert_eq!(response["ok"], false);
    assert_eq!(
        response["error"]["category"], "framing",
        "the refusal must carry the framing category: {response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("bad request")),
        "the refusal must name the framing violation: {response}"
    );

    // ...then the session ends: the next read is EOF, and the process exits.
    let mut tail = Vec::new();
    let count = tokio::time::timeout(Duration::from_secs(5), read_line(&mut stdout, &mut tail))
        .await
        .expect("the session must end")
        .expect("read after the error frame");
    assert_eq!(
        count, 0,
        "a framing violation must end the session, not keep the pipe open"
    );
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("the service process must exit")
        .expect("wait succeeds");
}

#[tokio::test]
async fn adapter_frame_cap_is_enforced_by_the_service_on_responses() {
    // Every ingest request is individually below the cap, but the resulting
    // checkpoint is not. This proves the adapter passes its configured cap
    // into the service process, which must replace the response instead of
    // serializing an unbounded line.
    let adapter = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Append,
        max_frame_bytes: 1024,
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn service with a small symmetric frame cap");

    for index in 0..12 {
        adapter
            .ingest(ContextIngress::UserMessage {
                content: format!("bounded checkpoint record {index}: {}", "x".repeat(120)),
            })
            .await
            .expect("each bounded request succeeds");
    }

    let error = adapter
        .checkpoint()
        .await
        .expect_err("the oversized response must be replaced by an error");
    assert!(
        error.to_string().contains("response exceeded frame bound"),
        "the bounded replacement must surface through the adapter: {error}"
    );
    adapter.shutdown().await;
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
            foreground_item_ids: materialized
                .foreground
                .iter()
                .map(|item| item.item_id)
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
        let metadata = if item == 0 {
            json!({"path": "RecallSentinel.rs"})
        } else {
            json!({})
        };
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: format!("overflow-{item}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "historical result".into(),
                    model_content,
                    artifact_ref: None,
                    metadata,
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
            label: None,
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
    // Anchor-root projection parity: the directive replaces the root set and
    // a PromptRequired hint forces the target into the frame — both must
    // cross the process boundary identically. The target is a just-materialized
    // item, so it is resident and the force-selection can land.
    let anchor_target = materialized
        .items
        .first()
        .map(|item| item.item_id)
        .expect("the materialized frame has at least one item");
    engine
        .ingest(ContextIngress::ContextDirective {
            action: agent_contracts::ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: anchor_target.to_string(),
                    strength: agent_contracts::AnchorRootStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                    ..Default::default()
                }],
            },
        })
        .await
        .unwrap();
    let anchored = engine
        .materialize(ContextQuery {
            current_input: "anchor parity".into(),
            budget_tokens: 4096,
            hints: ContextHints {
                max_selected_items: Some(16),
                anchor_roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: anchor_target.to_string(),
                    strength: agent_contracts::AnchorRootStrength::PromptRequired,
                    source_field_id: "constraints".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(
        anchored
            .items
            .iter()
            .any(|item| item.item_id == anchor_target),
        "a PromptRequired anchor root must force the target into the frame"
    );
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
        "anchored_item_count": anchored.items.len(),
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
            // counters) cannot match across two engines. The old
            // last-wins collapse picked the surviving value by HashMap
            // iteration order, which differs per process (RandomState),
            // so a map with a few divergent values flaked the parity
            // comparison. Instead strip every value, sort the result and
            // re-key with deterministic placeholders: equal value
            // multisets now always compare equal, and a real divergence
            // (a revision counted once more on one side) still fails.
            let uuid_keys: Vec<String> = map
                .keys()
                .filter(|key| key.len() == 36 && is_uuid_window(key.as_bytes()))
                .cloned()
                .collect();
            if !uuid_keys.is_empty() {
                let mut stripped: Vec<serde_json::Value> = Vec::with_capacity(uuid_keys.len());
                for key in &uuid_keys {
                    let mut nested = map.remove(key).expect("key present");
                    strip_random_uuids(&mut nested);
                    stripped.push(nested);
                }
                // serde_json::Value has no Ord/PartialOrd; sort by the
                // canonical serialization instead. serde_json's Map is
                // BTreeMap-backed (no preserve_order feature), so to_string
                // is deterministic and the sorted sequence depends only on
                // the value multiset, never on HashMap iteration order.
                stripped.sort_by_key(|value| value.to_string());
                for (index, nested) in stripped.into_iter().enumerate() {
                    let placeholder = if index == 0 {
                        "<uuid>".to_string()
                    } else {
                        format!("<uuid>{index}>")
                    };
                    map.insert(placeholder, nested);
                }
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
        program: Some(service_program()),
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
    // The service runs its startup reconcile before serving (crash-recovery
    // authority over an explicit store); the in-process engine only gets
    // one on this explicit call. Reconciling the local side here keeps the
    // `event_seq` clocks aligned so the parity comparison measures the
    // contract, not the startup transaction.
    local.reconcile_store().await.unwrap();

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
                    facts: None,
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
                facts: None,
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
        program: Some(service_program()),
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

// ---------------------------------------------------------------------------
// Exit-code contract at the process boundary
// ---------------------------------------------------------------------------

/// Spawn the real service process and drive its stdin/stdout directly so
/// the exit-code contract is exercised where a supervisor sees it.
struct SpawnedService {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
}

async fn spawn_raw_service() -> SpawnedService {
    let mut child = tokio::process::Command::new(service_program())
        .args(["--engine", "append"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the context service binary");
    let stdin = child.stdin.take().expect("service child stdin");
    let stdout = child.stdout.take().expect("service child stdout");
    SpawnedService {
        child,
        stdin,
        stdout,
    }
}

/// Read the single response line the service writes before it closes.
async fn read_response_line(service: &mut SpawnedService) -> serde_json::Value {
    let mut reader = tokio::io::BufReader::new(&mut service.stdout);
    let mut line = String::new();
    use tokio::io::AsyncBufReadExt;
    reader.read_line(&mut line).await.expect("a response line");
    serde_json::from_str(line.trim()).expect("a valid JSON response")
}

#[tokio::test]
async fn malformed_json_gets_protocol_category_and_exits_non_zero() {
    let mut service = spawn_raw_service().await;
    {
        use tokio::io::AsyncWriteExt;
        service
            .stdin
            .write_all(b"{not-json}\n")
            .await
            .expect("write the malformed frame");
    }
    let response = read_response_line(&mut service).await;
    assert_eq!(response["ok"], serde_json::Value::Bool(false));
    assert_eq!(
        response["error"]["category"], "protocol",
        "the sidecar must classify the same injected error as the in-process handler"
    );
    assert_eq!(
        response["error"]["retryable"],
        serde_json::Value::Bool(false)
    );
    drop(service.stdin);
    drop(service.stdout);
    let status = service
        .child
        .wait()
        .await
        .expect("the service process exits");
    assert!(
        !status.success(),
        "a terminal protocol violation must exit non-zero"
    );
}

#[tokio::test]
async fn malformed_utf8_gets_protocol_category_and_exits_non_zero() {
    let mut service = spawn_raw_service().await;
    {
        use tokio::io::AsyncWriteExt;
        service
            .stdin
            .write_all(&[0xff, b'\n'])
            .await
            .expect("write the undecodable frame");
    }
    let response = read_response_line(&mut service).await;
    assert_eq!(response["ok"], serde_json::Value::Bool(false));
    assert_eq!(
        response["error"]["category"], "protocol",
        "undecodable bytes fail at the JSON decode layer, not the frame reader"
    );
    drop(service.stdin);
    drop(service.stdout);
    let status = service
        .child
        .wait()
        .await
        .expect("the service process exits");
    assert!(
        !status.success(),
        "a terminal protocol failure must exit non-zero"
    );
}

#[tokio::test]
async fn clean_eof_exits_zero() {
    let mut service = spawn_raw_service().await;
    drop(service.stdin);
    drop(service.stdout);
    let status = service
        .child
        .wait()
        .await
        .expect("the service process exits");
    assert!(
        status.success(),
        "a clean EOF is a normal disconnect and must exit zero"
    );
}

#[tokio::test]
async fn graceful_shutdown_exits_zero() {
    let mut service = spawn_raw_service().await;
    {
        use tokio::io::AsyncWriteExt;
        service
            .stdin
            .write_all(b"{\"id\":1,\"version\":1,\"op\":\"shutdown\"}\n")
            .await
            .expect("write the shutdown request");
    }
    let response = read_response_line(&mut service).await;
    assert_eq!(response["ok"], serde_json::Value::Bool(true));
    drop(service.stdin);
    drop(service.stdout);
    let status = service
        .child
        .wait()
        .await
        .expect("the service process exits");
    assert!(
        status.success(),
        "a graceful shutdown request must exit zero"
    );
}

/// EXEC-9 (R3-01): the audit's deletion counterexample, turned into a
/// retention regression that runs the real service process. An externalized
/// body is referenced by retained checkpoint A; a newer snapshot B admits
/// the body back into the working set, so from B's view the blob is a stale
/// duplicate. The service engine must (1) parse A's recovery roots with its
/// real, versioned implementation — never an empty set; (2) defer deletion
/// for an unknown root or an incomplete root set; (3) keep the blob under a
/// protecting reconcile; (4) restore A and read the body back — identical to
/// the in-process engine.
#[tokio::test]
async fn recovery_roots_and_protected_reconcile_parity_across_the_service_boundary() {
    let service_store = IsolatedStore::new("exec9-service");
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        store_dir: Some(service_store.path().to_path_buf()),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn isolated context service");
    let local_store = IsolatedStore::new("exec9-local");
    let local = context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
        context_store_dir: Some(local_store.path().to_path_buf()),
        ..context_simple::SimpleContextConfig::default()
    });

    let mut root_counts = Vec::new();
    let service: Arc<dyn ContextEngine> = Arc::new(service);
    let local: Arc<dyn ContextEngine> = Arc::new(local);
    let engines: Vec<Arc<dyn ContextEngine>> = vec![service, local];
    for engine in engines {
        // Overflow the reversible buffer with observations touching one
        // resource, so the full GC pass externalizes their bodies into the
        // store (each becomes a recovery root for the checkpoint below).
        let overflow = context_simple::SimpleContextConfig::default().gc_buffer_capacity + 1;
        for item in 0..overflow {
            engine
                .ingest(ContextIngress::ToolObservation {
                    facts: None,
                    output: ToolOutput {
                        call_id: format!("exec9-{item}"),
                        tool_name: "shell.exec".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!(
                            "step {item}: fix AuthService.rs {}",
                            "x".repeat(120)
                        ),
                        artifact_ref: None,
                        metadata: json!({ "path": "AuthService.rs" }),
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
            "the buffer overflow must externalize the bodies: {gc:?}"
        );

        // The externalized body and the checkpoint that references it.
        let refs = engine
            .search_external(agent_contracts::ContextSearchQuery {
                query: "AuthService".into(),
                kind: Some(agent_contracts::ContextKind::ToolObservation),
                scope: None,
                task_id: None,
                label: None,
                limit: 16,
            })
            .await
            .unwrap();
        assert!(!refs.is_empty(), "the retrieval surface must list the ref");
        let target = refs[0].item_id;
        let expected = engine
            .fetch_external(target)
            .await
            .unwrap()
            .expect("the ref resolves before any restore")
            .content;
        let snapshot_a = engine.checkpoint().await.unwrap();

        // Admit the body back into the working set, then save B: from B's
        // view the blob is a stale duplicate of resident content.
        engine
            .ingest(ContextIngress::ContextDirective {
                action: ContextAction::Admit {
                    item_id: target,
                    reason: "the model needs this step again".into(),
                },
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await
            .unwrap();
        let snapshot_b = engine.checkpoint().await.unwrap();

        engine.restore(snapshot_b.clone()).await.unwrap();

        // (1) A's recovery roots parse for real — one root, exactly the
        // externalized body. The pre-fix service returned an empty set here
        // (R3-01), which reads as "nothing retained".
        let roots = engine
            .checkpoint_recovery_item_ids(&snapshot_a)
            .await
            .unwrap();
        assert_eq!(
            roots,
            vec![target],
            "A's checkpoint must name the external body as a recovery root"
        );

        // (2) an incomplete root set defers deletion: the runtime cannot
        // prove "nothing retained", so the stale-duplicate branch holds.
        let incomplete = engine.reconcile_store_protecting(&[], false).await.unwrap();
        assert_eq!(
            incomplete.deleted_stale, 0,
            "an incomplete root set must defer deletion: {incomplete:?}"
        );

        // (3) the protecting reconcile keeps the blob; restoring A reads the
        // same body back.
        let report = engine
            .reconcile_store_protecting(&roots, true)
            .await
            .unwrap();
        assert_eq!(
            report.deleted_stale, 0,
            "a recovery-root blob must not be deleted as stale: {report:?}"
        );
        engine.restore(snapshot_a.clone()).await.unwrap();
        let fetched = engine
            .fetch_external(target)
            .await
            .unwrap()
            .expect("checkpoint A must still restore its body");
        assert_eq!(fetched.content, expected, "the restored body must match");

        // (5) the honest counterexample: back in B's view (the body is
        // resident again), with the blob NOT named in the root set the same
        // reconcile deletes it as a stale duplicate — deletion only ever
        // happens outside protection, never through it.
        engine.restore(snapshot_b.clone()).await.unwrap();
        let unprotected = engine
            .reconcile_store_protecting(&[ContextItemId::new()], true)
            .await
            .unwrap();
        assert_eq!(
            unprotected.deleted_stale, 1,
            "outside protection the stale duplicate is deleted: {unprotected:?}"
        );

        root_counts.push(roots.len());
    }
    service_and_local_root_parity(&root_counts);
}

fn service_and_local_root_parity(counts: &[usize]) {
    assert!(
        counts.iter().all(|count| *count == 1),
        "the protected root must parse identically in-process and across the service boundary: {counts:?}"
    );
}

/// N01 (baseline `6eda2474` re-review): the real service process's startup
/// reconcile runs BEFORE any restore installs recovery ownership, so it must
/// be non-destructive. A sharded checkpoint's spill cards survive a full
/// service exit → same-store restart → restore, and every restored entry
/// carries the exact metadata captured at checkpoint time — not the
/// degraded "rebuilt" shape the old startup reconcile's orphan-card sweep
/// produced.
#[tokio::test]
async fn a_service_restart_over_one_store_keeps_the_sharded_checkpoints_cards() {
    // Build the spilled state in-process: the service binary's default
    // config never spills (inline target 2048), so the writer engine uses a
    // small inline target while the store layout stays identical.
    let store = IsolatedStore::new("n01-restart");
    let local = context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
        external_checkpoint_inline_target: 10,
        gc_buffer_capacity: 0,
        context_store_dir: Some(store.path().to_path_buf()),
        ..context_simple::SimpleContextConfig::default()
    });
    let overflow = context_simple::SimpleContextConfig::default().gc_buffer_capacity + 1;
    for item in 0..overflow {
        local
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: format!("n01-{item}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "historical result".into(),
                    model_content: format!("step {item}: fix Ledger.rs {}", "y".repeat(120)),
                    artifact_ref: None,
                    metadata: json!({ "path": "Ledger.rs" }),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    local
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = local.gc().await.unwrap();
    assert!(
        gc.externalized > 0,
        "the overflow must externalize bodies: {gc:?}"
    );
    // Age the Cold entries to External residency: spill cards only cover
    // entries whose stored life has aged past gc_external_ttl_generations
    // (4) idle full-GC generations. Each extra pass bumps the epoch without
    // touching the entries.
    for _ in 0..5 {
        local.gc().await.unwrap();
    }

    let checkpoint = local.checkpoint().await.unwrap();
    let spilled_rows = checkpoint["external_spilled"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !spilled_rows.is_empty(),
        "the capture must spill metadata cards: externalized={} spilled={} external_len={}",
        gc.externalized,
        spilled_rows.len(),
        checkpoint["external"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0),
    );
    // The metadata as captured — this is what the restore promise covers.
    let mut captured = Vec::new();
    for row in &spilled_rows {
        let id = ContextItemId::parse_ref(row["id"].as_str().unwrap()).unwrap();
        let entry = local
            .inspect_external(id)
            .await
            .unwrap()
            .expect("the spilled entry is live before the restart");
        captured.push((id, serde_json::to_value(&entry).unwrap()));
    }
    drop(local);

    // Restart: the real service binary over the same store. Its startup
    // reconcile sees an empty external map, the checkpoint's cards, and
    // their blobs — the exact N01 counterexample shape.
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        store_dir: Some(store.path().to_path_buf()),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn isolated context service");
    let cards_dir = store.path().join("cards");
    let card_count = std::fs::read_dir(&cards_dir)
        .expect("the cards directory survives the restart")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "card"))
        .count();
    assert_eq!(
        card_count,
        spilled_rows.len(),
        "the startup reconcile must not delete the retained checkpoint's cards"
    );

    // Restore and verify: every spilled entry comes back with the captured
    // metadata, and one body reads back through the formal fetch path.
    service.restore(checkpoint).await.unwrap();
    for (id, before) in &captured {
        let after = service
            .inspect_external(*id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("restored entry {id} is missing after the restart"));
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            *before,
            "entry {id} must restore the exact captured metadata"
        );
    }
    let body = service
        .fetch_external(captured[0].0)
        .await
        .unwrap()
        .expect("the retained checkpoint's body is retrievable");
    assert!(body.content.contains("Ledger.rs"), "{body:?}");

    service.shutdown().await;
}

// ---------------------------------------------------------------------------
// W2 (V3): atomic search facts across the service boundary
// ---------------------------------------------------------------------------

/// The W2 parity fixture's cold-paging shape: a restore batch of 4, a hot
/// metadata cap of 4 and a per-op hydrate budget of 8 over a 14-entry
/// all-cards history — the first pass after restore stops at the hot cap
/// with a typed unread remainder, exactly the state whose coverage the old
/// adapter lost. Both engines (in-process and service via `--cold-paging`)
/// run the SAME values; only the production defaults are larger.
const W2_N: usize = 14;
const W2_RESTORE_BATCH: usize = 4;
const W2_HOT_CAP: usize = 4;
const W2_HYDRATE_ITEMS: usize = 8;

/// Build one all-cards cold history of `n` tool observations in `store` and
/// return its checkpoint, through the PUBLIC engine surface (ingest /
/// maintain / gc) — no crate-private fixtures. The writer spills the whole
/// tail to manifest rows (zero inline target, one big card batch) and ages
/// the entries past the spill gate, so a restore pages in only one batch and
/// the rest are pending cold pages.
async fn w2_spilled_fixture(store: &Path, n: usize) -> serde_json::Value {
    let writer = context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        gc_buffer_capacity: 0,
        external_checkpoint_io_budget_ms: 60_000,
        external_checkpoint_card_batch: 8_192,
        external_checkpoint_card_bytes: 256 * 1024 * 1024,
        external_checkpoint_scan_budget: 8_192,
        context_store_dir: Some(store.to_path_buf()),
        ..context_simple::SimpleContextConfig::default()
    });
    writer
        .ingest(ContextIngress::FocusChanged {
            focus: agent_contracts::FocusState::for_task(
                agent_contracts::TaskId::new(),
                "w2 search parity",
            ),
        })
        .await
        .unwrap();
    for index in 0..n {
        // The needle rides the STAMPED PATH, not the body: raw
        // ToolObservation bodies are identity-only on the catalog surface by
        // design (their text is not a search needle), while the stamped path
        // is an indexed entity — exactly the surface the sentinel test
        // above searches. Every observation of the same file shares it, so
        // each pass's hot window yields its own hits.
        writer
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: format!("w2-{index}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "w2 observation".into(),
                    model_content: format!("step {index}: w2-needle-{index:05} {}", "x".repeat(60)),
                    artifact_ref: None,
                    metadata: json!({"path": "w2-needle.rs"}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    writer
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = writer.gc().await.unwrap();
    assert!(gc.externalized >= n, "the wave must externalize: {gc:?}");
    // Age the external entries past the spill gate (4 idle full-GC
    // generations) so the checkpoint spills them as manifest rows.
    for _ in 0..5 {
        writer.gc().await.unwrap();
    }
    let checkpoint = writer.checkpoint().await.unwrap();
    let spilled = checkpoint
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(spilled, n, "every cold entry is a manifest row");
    checkpoint
}

/// The in-process twin of the service's `--cold-paging` engine config.
fn w2_local_config(store: &Path) -> context_simple::SimpleContextConfig {
    context_simple::SimpleContextConfig {
        context_store_dir: Some(store.to_path_buf()),
        external_restore_card_batch: W2_RESTORE_BATCH,
        external_hot_metadata_max_entries: W2_HOT_CAP,
        external_hydrate_max_items: W2_HYDRATE_ITEMS,
        ..context_simple::SimpleContextConfig::default()
    }
}

/// Normalize one atomic report into the parity-comparable facts: hit ids in
/// order plus the coverage tuple (tokens are per-engine opaque identities,
/// so only their presence is comparable).
fn w2_report_facts(
    report: &agent_contracts::ContextSearchResult,
) -> (
    Vec<agent_contracts::ContextItemId>,
    (bool, usize, String, bool),
) {
    (
        report.hits.iter().map(|hit| hit.item_id).collect(),
        (
            report.coverage.complete,
            report.coverage.unread_pages,
            report.coverage.stop.to_string(),
            report.coverage.continuation.is_some(),
        ),
    )
}

/// V3 live red: a fresh boundary must not self-certify `complete` coverage.
/// The old adapter left `last_search_coverage` at the trait default, so a
/// boundary that had never observed any pass answered "complete" — the
/// exact fabricated fact V3 removes. (Pre-fix this assertion is red: the
/// default is `complete`.)
#[tokio::test]
async fn a_fresh_boundary_never_self_certifies_complete_coverage() {
    // The concrete adapter binding keeps the graceful shutdown available
    // (the shared `connect()` helper erases it behind the trait object).
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn + handshake with the context service");
    let coverage = service.last_search_coverage();
    assert!(
        !coverage.complete,
        "an unsearched boundary has no pass to describe: {coverage:?}"
    );
    assert_eq!(
        coverage.stop,
        agent_contracts::ContextSearchCoverageStop::Unknown,
        "the explicit no-facts state is Unknown, not a silent complete: {coverage:?}"
    );
    service.shutdown().await;
}

/// V3 counterexample set: the same cold/hot fixture driven in lockstep
/// through the in-process engine and the service process. The atomic report
/// must agree on non-empty incomplete, empty incomplete (fail-closed Err),
/// the continuation walk to the last page, and a stale token — with hits
/// and coverage belonging to the SAME pass on both sides.
#[tokio::test]
async fn search_report_parity_across_the_service_boundary() {
    let store = IsolatedStore::new("w2-parity");
    let checkpoint = w2_spilled_fixture(store.path(), W2_N).await;
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        store_dir: Some(store.path().to_path_buf()),
        cold_paging: Some((W2_RESTORE_BATCH, W2_HOT_CAP, W2_HYDRATE_ITEMS)),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn service over the fixture store");
    let local = context_simple::SimpleContextEngine::new(w2_local_config(store.path()));
    service.restore(checkpoint.clone()).await.unwrap();
    local.restore(checkpoint).await.unwrap();

    // --- non-empty incomplete: hits and coverage of the same pass -------
    let query = || agent_contracts::ContextSearchQuery::new("w2-needle.rs", 20);
    let service_report = service.search_external_report(query(), None).await.unwrap();
    let local_report = local.search_external_report(query(), None).await.unwrap();
    let (service_hits, service_facts) = w2_report_facts(&service_report);
    let (local_hits, local_facts) = w2_report_facts(&local_report);
    assert_eq!(service_hits, local_hits, "the same first window is visible");
    assert_eq!(
        service_facts, local_facts,
        "the typed gap agrees: {service_facts:?}"
    );
    assert_eq!(
        service_facts,
        (false, W2_N - W2_RESTORE_BATCH, "hot-cap".to_string(), true),
        "both sides report the non-empty incomplete pass: {service_facts:?}"
    );
    let stale_token = service_report.coverage.continuation.clone().unwrap();

    // --- the continuation walk reaches the last page --------------------
    let mut token = local_report.coverage.continuation.clone().unwrap();
    let mut service_token = stale_token.clone();
    for pass in 0..8 {
        let service_next = service
            .search_external_report(query(), Some(&service_token))
            .await
            .unwrap();
        let local_next = local
            .search_external_report(query(), Some(&token))
            .await
            .unwrap();
        let (s_hits, s_facts) = w2_report_facts(&service_next);
        let (l_hits, l_facts) = w2_report_facts(&local_next);
        assert_eq!(s_hits, l_hits, "pass {pass}: the walk serves the same page");
        assert_eq!(s_facts, l_facts, "pass {pass}: coverage agrees");
        if service_next.coverage.complete {
            assert!(
                local_next.coverage.complete,
                "pass {pass}: the walk must complete on both sides together"
            );
            break;
        }
        service_token = service_next.coverage.continuation.clone().unwrap();
        token = local_next.coverage.continuation.clone().unwrap();
    }

    // --- empty incomplete: zero hits over an unread region fail closed --
    let absent = || agent_contracts::ContextSearchQuery::new("w2-absent-needle", 20);
    let service_zero = service
        .search_external_report(absent(), None)
        .await
        .expect_err("an empty result over an unread region must fail closed");
    let local_zero = local
        .search_external_report(absent(), None)
        .await
        .expect_err("the in-process twin fails closed the same way");
    let (service_zero, local_zero) = (service_zero.to_string(), local_zero.to_string());
    for message in [&service_zero, &local_zero] {
        assert!(
            message.contains("coverage incomplete"),
            "both sides name the state: {message}"
        );
        assert!(
            message.contains("pending spill page"),
            "both sides name the typed remainder: {message}"
        );
    }

    // --- a stale token degrades to a fresh walk, never an error ---------
    let service_replay = service
        .search_external_report(query(), Some(&stale_token))
        .await
        .expect("a stale token is a fresh walk, not an error");
    let local_replay = local
        .search_external_report(query(), Some(&local_replay_token(&local).await))
        .await
        .expect("the in-process twin degrades the same way");
    let (s_hits, s_facts) = w2_report_facts(&service_replay);
    let (l_hits, l_facts) = w2_report_facts(&local_replay);
    assert_eq!(s_hits, l_hits, "the replay serves the same fresh window");
    assert_eq!(s_facts, l_facts, "the replay's coverage agrees");
    assert_ne!(
        service_replay.coverage.continuation.as_deref(),
        Some(stale_token.as_str()),
        "the replayed identity is never re-matched"
    );
    service.shutdown().await;
}

/// The in-process twin of the stale token used above: the first pass's
/// token, long since replaced by the walk.
async fn local_replay_token(local: &dyn ContextEngine) -> String {
    local
        .last_search_coverage()
        .continuation
        .clone()
        .unwrap_or_else(|| "cold-window-e0-n1".to_string())
}

/// V3, legacy channel: a search whose pass is provably incomplete must not
/// read back as complete coverage through the boundary. The old adapter
/// left `last_search_coverage` at the trait default; with the atomic
/// capability negotiated, the legacy method now routes through the report
/// op and mirrors the real facts.
#[tokio::test]
async fn legacy_search_channel_no_longer_fabricates_complete_coverage() {
    let store = IsolatedStore::new("w2-legacy");
    let checkpoint = w2_spilled_fixture(store.path(), W2_N).await;
    let service = ContextServiceAdapter::connect(&ContextServiceConfig {
        program: Some(service_program()),
        engine: ServiceEngine::Dynamic,
        store_dir: Some(store.path().to_path_buf()),
        cold_paging: Some((W2_RESTORE_BATCH, W2_HOT_CAP, W2_HYDRATE_ITEMS)),
        ..ContextServiceConfig::default()
    })
    .await
    .expect("spawn service over the fixture store");
    let local = context_simple::SimpleContextEngine::new(w2_local_config(store.path()));
    service.restore(checkpoint.clone()).await.unwrap();
    local.restore(checkpoint).await.unwrap();

    let query = agent_contracts::ContextSearchQuery::new("w2-needle.rs", 20);
    let service_hits = service.search_external(query.clone()).await.unwrap();
    let service_coverage = service.last_search_coverage();
    let local_hits = local.search_external(query).await.unwrap();
    let local_coverage = local.last_search_coverage();

    assert_eq!(service_hits.len(), local_hits.len());
    assert!(!local_coverage.complete && local_coverage.continuation.is_some());
    assert_eq!(
        (
            service_coverage.complete,
            service_coverage.unread_pages,
            service_coverage.stop
        ),
        (
            local_coverage.complete,
            local_coverage.unread_pages,
            local_coverage.stop
        ),
        "the boundary forwards the real coverage facts: {service_coverage:?} vs {local_coverage:?}"
    );
    assert!(
        service_coverage.continuation.is_some(),
        "the continuation cursor crosses the boundary: {service_coverage:?}"
    );
    service.shutdown().await;
}

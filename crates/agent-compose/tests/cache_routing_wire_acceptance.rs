//! C1/C2 KV wiring — wire acceptance over the REAL production chain, not a
//! hand-filled DTO: `build_context_engine` / `compose` assemble the runtime,
//! the actor stamps the routing key at its single production call site, and
//! the real OpenAI Responses transport emits HTTP against a local capture
//! server. Nothing in this file writes `prompt_cache_key`,
//! `prompt_cache_options` or `prompt_cache_breakpoint` itself.
//!
//! Covered here beyond `cache_wire_flow.rs`:
//! - the key's exact composed shape `{isolation}|{workspace}|{endpoint}|{task}|{lane}`
//!   (with an OPAQUE workspace identity — the composed runtime owns the real
//!   workspace root, and the key must never leak that plaintext path);
//! - different task => different key, maintenance lane => different key
//!   (`...|compaction|maintenance`, its own ExplicitOnly ZERO-breakpoint
//!   no-write shape and the compaction output cap on the wire);
//! - multi-breakpoint mapping: B0 stable policy / B1 declared epoch evidence
//!   are per-item sibling fields whose contents really separate the stable
//!   prefix from the volatile tail (the current turn text sits after B1);
//! - an empty stable set declares ONLY B0 in the DECLARED sibling form —
//!   never the legacy whole-frame `None` shape whose breakpoint rides inside
//!   a rewritten content array;
//! - an endpoint that never confirmed the capability (ProviderDefault)
//!   strips every cache field from the wire even though the runtime stamped
//!   the request.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use agent_compose::{
    ComposeConfig, ContextPolicy, MaintenanceBudget, build_context_engine, compose,
};
use agent_contracts::{
    COMPACTION_OUTPUT_CHARS, ContextIngress, ContextMaintenanceTrigger, PromptCacheRouting,
    RuntimeEvent, RuntimeEventEnvelope, TaskId,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use provider_openai::{OpenAiConfig, OpenAiPromptCacheMode, OpenAiProtocol, OpenAiProvider};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Windows system-proxy environments intercept loopback unless told not to
/// (documented repo-wide); set once per process, before any request.
static NO_PROXY: Once = Once::new();

fn allow_loopback_direct() {
    NO_PROXY.call_once(|| {
        // Safety: single-threaded one-shot env setup before any request.
        unsafe { std::env::set_var("NO_PROXY", "127.0.0.1,localhost") };
    });
}

const ISOLATION: &str = "acceptance-isolation";
const TURN_ONE: &str = "first turn of the acceptance routing check";
const TURN_TWO: &str = "second turn of the acceptance routing check";

/// A persistent capture server: one SSE Responses stream per POST (plain
/// text answer), every request body appended to the shared log. The rolling
/// engine's own fold cadence decides how many maintenance rounds appear and
/// when, so round classification is content-based, never positional.
async fn spawn_capture_server(bodies: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let bodies = Arc::clone(&bodies);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                // Read to the end of the request body (parse headers and
                // read content-length; half-close is not guaranteed).
                let mut read = vec![0u8; 16 * 1024];
                while let Ok(n) = socket.read(&mut read).await {
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&read[..n]);
                    if let Some(header_end) = find_subsequence(&buffer, b"\r\n\r\n") {
                        let length = buffer[header_end + 4..]
                            .len()
                            .max(content_length(&buffer[..header_end + 4]));
                        if buffer[header_end + 4..].len() >= length {
                            break;
                        }
                    }
                }
                let body_start = find_subsequence(&buffer, b"\r\n\r\n")
                    .map(|index| index + 4)
                    .unwrap_or(0);
                let body = String::from_utf8_lossy(&buffer[body_start..]).to_string();
                bodies.lock().unwrap().push(body);

                let delta = json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": "done",
                });
                let completed = json!({
                    "type": "response.completed",
                    "response": {"usage": {"input_tokens": 50, "output_tokens": 5}}
                });
                let sse_body = format!(
                    "event: response.output_text.delta\r\ndata: {}\r\n\r\nevent: response.completed\r\ndata: {}\r\n\r\n",
                    delta, completed
                );
                let sse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
                );
                let _ = socket.write_all(sse.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr.port()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
}

/// A real Responses transport over the local capture server. `cache_mode` is
/// the endpoint-capability knob under test: only a confirmed explicit
/// profile may add the cache fields to the wire.
fn wire_provider(
    base_url: String,
    cache_mode: OpenAiPromptCacheMode,
) -> Arc<dyn agent_contracts::ModelTransport> {
    let config = OpenAiConfig {
        api_key: "test-key".into(),
        base_url,
        model: "capture-model".into(),
        protocol: OpenAiProtocol::Responses,
        max_output_tokens: 256,
        timeout: Duration::from_secs(10),
        send_stream_options: false,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
        context_window: Some(64_000),
        sampling: provider_openai::SamplingPolicy::ProviderDefault,
    };
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let provider = match cache_mode {
        OpenAiPromptCacheMode::ResponsesExplicit => OpenAiProvider::with_client(config, client)
            .with_prompt_cache_mode(cache_mode)
            .expect("responses protocol supports the explicit cache mode"),
        OpenAiPromptCacheMode::ProviderDefault => OpenAiProvider::with_client(config, client),
    };
    Arc::new(provider)
}

/// The operator-side opaque workspace identity: a digest of the root path,
/// exactly the "canonical root OR ITS DIGEST" form the routing contract
/// allows. The composition runtime still owns the real workspace; the key
/// must never leak the plaintext path.
fn opaque_workspace_identity(root: &std::path::Path) -> String {
    let digest = Sha256::digest(root.display().to_string().as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("sha256-{hex}")
}

fn routing_for(
    isolation: &str,
    workspace_identity: String,
    endpoint: String,
) -> PromptCacheRouting {
    PromptCacheRouting {
        isolation: isolation.to_string(),
        workspace: workspace_identity,
        endpoint,
    }
}

/// Flattened text of one wire input item (string content, content-part
/// array, or function_call_output text).
fn item_text(item: &Value) -> String {
    if let Some(text) = item["content"].as_str() {
        return text.to_string();
    }
    if let Some(parts) = item["content"].as_array() {
        return parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
    }
    item["output"].as_str().unwrap_or_default().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakpointForm {
    /// `input[i].prompt_cache_breakpoint` — the DECLARED per-item shape.
    SiblingField,
    /// `input[i].content[k].prompt_cache_breakpoint` — the LEGACY
    /// whole-frame reuse-boundary shape (rewritten content array).
    ContentPart,
}

fn item_breakpoint_form(item: &Value) -> Option<BreakpointForm> {
    let on_content = item["content"].as_array().is_some_and(|parts| {
        parts
            .iter()
            .any(|part| part.get("prompt_cache_breakpoint").is_some())
    });
    let on_item = item.get("prompt_cache_breakpoint").is_some();
    if on_content {
        Some(BreakpointForm::ContentPart)
    } else if on_item {
        Some(BreakpointForm::SiblingField)
    } else {
        None
    }
}

/// (input index, form) of every breakpoint-carrying item, in wire order.
fn breakpoint_positions(body: &Value) -> Vec<(usize, BreakpointForm)> {
    body["input"]
        .as_array()
        .expect("every captured body is a responses request with input")
        .iter()
        .enumerate()
        .filter_map(|(index, item)| item_breakpoint_form(item).map(|form| (index, form)))
        .collect()
}

/// Recursive scan: does any nested object carry one of the field names?
fn carries_any_field(value: &Value, fields: &[&str]) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(name, nested)| {
            fields.contains(&name.as_str()) || carries_any_field(nested, fields)
        }),
        Value::Array(items) => items.iter().any(|item| carries_any_field(item, fields)),
        _ => false,
    }
}

async fn wait_for_turn_completed(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete within the test deadline"
        );
        match events.try_recv() {
            Ok(envelope) => {
                if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("the event stream closed before TurnCompleted")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_focus(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
    goal_marker: &str,
) -> TaskId {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no FocusChanged for {goal_marker} within the test deadline"
        );
        match events.try_recv() {
            Ok(envelope) => {
                if let RuntimeEvent::FocusChanged { task_id, goal } = &envelope.event
                    && goal.contains(goal_marker)
                {
                    return *task_id;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("the event stream closed before FocusChanged({goal_marker})")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn routing_keys_route_by_task_and_lane_on_the_wire() {
    allow_loopback_direct();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_capture_server(Arc::clone(&bodies)).await;
    let base_url = format!("http://127.0.0.1:{port}/v1");

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let root_path = workspace.root().display().to_string();
    let workspace_identity = opaque_workspace_identity(workspace.root());
    let routing = routing_for(ISOLATION, workspace_identity.clone(), base_url.clone());

    // Same derivation as the host/TUI composition roots (main.rs): the
    // maintenance lane gets its own stable namespace key.
    let maintenance_key = Some(routing.key_for("compaction", "maintenance"));
    let provider = wire_provider(base_url.clone(), OpenAiPromptCacheMode::ResponsesExplicit);
    let context_engine = build_context_engine(
        ContextPolicy::Rolling,
        workspace.state_dir(),
        Some(Arc::clone(&provider)),
        None,
        &MaintenanceBudget::default(),
        maintenance_key,
    )
    .await
    .unwrap();

    // Fire the MAINTENANCE lane through the composed engine's real
    // compactor (production `build_context_engine` wiring) BEFORE any turn:
    // cross the rolling fold threshold so the next maintain runs it.
    for index in 0..40 {
        context_engine
            .ingest(ContextIngress::AssistantMessage {
                content: format!("history record {index}: {}", "detail ".repeat(140)),
            })
            .await
            .unwrap();
    }
    let report = context_engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    assert!(
        !report.compactions.is_empty(),
        "the rolling fold must fire so the maintenance lane reaches the wire"
    );

    let config = ComposeConfig {
        provider_profile_digest: None,
        cache_routing: Some(routing),
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model: provider,
        approval: Arc::new(PolicyApprovalGate::read_only()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: None,
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: None,
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    };
    let composed = compose(config).await.unwrap();
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();
    let mut events = composed.subscribe();

    // Task ALPHA: two turns — the second turn's request differs in its
    // volatile tail while the routing key and declared prefix stay put.
    // Task BETA: a different goal => a different task identity.
    handle
        .set_focus("alpha goal for the routing check".into())
        .await
        .unwrap();
    let task_a = wait_for_focus(&mut events, "alpha goal").await;
    handle.user_message(TURN_ONE.into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    handle.user_message(TURN_TWO.into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;

    handle
        .set_focus("beta goal — a different task identity".into())
        .await
        .unwrap();
    let task_b = wait_for_focus(&mut events, "beta goal").await;
    assert_ne!(task_a, task_b, "a different goal must mint a new task id");
    handle
        .user_message("beta turn of the acceptance routing check".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let _ = composed.shutdown().await;

    let captured = bodies.lock().unwrap();
    let parsed: Vec<Value> = captured
        .iter()
        .map(|body| serde_json::from_str(body).expect("every captured body is responses JSON"))
        .collect();
    eprintln!("captured HTTP rounds: {}", parsed.len());
    assert!(
        parsed.len() >= 4,
        "the maintenance folds + alpha turn 1 + alpha turn 2 + beta must reach the wire, got {}",
        parsed.len()
    );

    // Classify the lanes by content: the compactor's system prompt is a
    // production constant; main-lane requests carry the tool surface.
    let is_compactor =
        |body: &Value| item_text(&body["input"].as_array().unwrap()[0]).contains("compress folded");
    let main_rounds: Vec<usize> = (0..parsed.len())
        .filter(|round| !is_compactor(&parsed[*round]))
        .collect();
    let compactor_rounds: Vec<usize> = (0..parsed.len())
        .filter(|round| is_compactor(&parsed[*round]))
        .collect();
    assert!(
        !main_rounds.is_empty() && !compactor_rounds.is_empty(),
        "both lanes must reach the wire (main: {:?}, compactor: {:?})",
        main_rounds,
        compactor_rounds
    );
    for (round, body) in parsed.iter().enumerate() {
        eprintln!(
            "round {round}: lane={} key={:?} breakpoints={:?} input_items={}",
            if is_compactor(body) {
                "maintenance"
            } else {
                "main"
            },
            body["prompt_cache_key"]
                .as_str()
                .map(|key| &key[key.chars().count().saturating_sub(40)..]),
            breakpoint_positions(body),
            body["input"].as_array().map(Vec::len).unwrap_or(0),
        );
    }

    // ---- Main lane: the exact composed key shape, stable per task ----
    let expected_main_key =
        |task: TaskId| format!("{ISOLATION}|{workspace_identity}|{base_url}|{task}|main");
    let mut keys_a = BTreeSet::new();
    let mut keys_b = BTreeSet::new();
    for &round in &main_rounds {
        let body = &parsed[round];
        let key = body["prompt_cache_key"]
            .as_str()
            .expect("every confirmed-capability main request carries the routing key");
        let components: Vec<&str> = key.split('|').collect();
        assert_eq!(
            components.len(),
            5,
            "the key is the five-component isolation|workspace|endpoint|task|lane composition: {key}"
        );
        assert_eq!(components[0], ISOLATION);
        assert_eq!(
            components[1], workspace_identity,
            "the configured OPAQUE workspace identity is the component, verbatim"
        );
        assert_eq!(components[2], base_url);
        assert_eq!(components[4], "main");
        let task = components[3]
            .parse::<TaskId>()
            .expect("the task component is the minted task id");
        assert!(
            task == task_a || task == task_b,
            "the task component names one of this run's tasks: {key}"
        );
        assert_eq!(
            key,
            expected_main_key(task),
            "the wire key is exactly the composed routing string"
        );
        assert!(
            !key.contains(&root_path),
            "the key must never leak the real workspace path the runtime owns: {key}"
        );
        assert!(
            key.parse::<TaskId>().is_err(),
            "the key is a composed routing string, never a bare per-round UUID: {key}"
        );
        assert_eq!(
            body["prompt_cache_options"]["mode"].as_str(),
            Some("explicit"),
            "the confirmed explicit capability is declared on the wire"
        );
        if task == task_a {
            keys_a.insert(key.to_string());
        } else {
            keys_b.insert(key.to_string());
        }
    }
    assert_eq!(
        keys_a.len(),
        1,
        "one task, one stable key across every round and turn (also proves the key is not a per-request hash of changing content)"
    );
    assert_eq!(keys_b.len(), 1, "task beta is stable too");
    assert_ne!(
        keys_a, keys_b,
        "different tasks must route under different keys"
    );

    // ---- Multi-breakpoint mapping: B0 stable policy, B1 declared epoch ----
    let multi_rounds: Vec<usize> = main_rounds
        .iter()
        .copied()
        .filter(|round| breakpoint_positions(&parsed[*round]).len() == 2)
        .collect();
    assert!(
        !multi_rounds.is_empty(),
        "once evidence exists, later rounds must declare BOTH breakpoints on the wire"
    );
    // The dynamic tail must track the CHANGING turn while the declared
    // boundaries stay put: rounds of turn 1 carry its text after B1, later
    // rounds of the same task carry the NEW turn's text there instead.
    let mut saw_turn_one_tail = false;
    let mut saw_turn_two_tail = false;
    for &round in &multi_rounds {
        let body = &parsed[round];
        let positions = breakpoint_positions(body);
        let input = body["input"].as_array().unwrap();
        let (b0, b0_form) = positions[0];
        let (b1, b1_form) = positions[1];
        assert!(b0 < b1, "B0 (stable policy end) precedes B1 (evidence end)");
        assert_eq!(
            (b0_form, b1_form),
            (BreakpointForm::SiblingField, BreakpointForm::SiblingField),
            "declared breakpoints map per item as sibling fields, never through the legacy content-array rewrite"
        );
        let b1_text = item_text(&input[b1]);
        assert!(
            b1_text.contains("SELECTED WORKING CONTEXT"),
            "B1 marks the declared epoch evidence block: {}",
            &b1_text[..b1_text.chars().count().min(120)]
        );
        let mut tail_text = String::new();
        for (index, item) in input.iter().enumerate().skip(b1 + 1) {
            assert!(
                item_breakpoint_form(item).is_none(),
                "no breakpoint may sit beyond the declared B1 boundary (round {round}, item {index})"
            );
            tail_text.push_str(&item_text(item));
        }
        saw_turn_one_tail |= tail_text.contains(TURN_ONE);
        saw_turn_two_tail |= tail_text.contains(TURN_TWO);
    }
    assert!(
        saw_turn_one_tail && saw_turn_two_tail,
        "the text after B1 must follow the changing turn (turn 1 tail: {saw_turn_one_tail}, turn 2 tail: {saw_turn_two_tail})"
    );
    // The B0 item's content is the STABLE policy prefix: identical on every
    // main round, including rounds whose suffix changed completely.
    let two_body = &parsed[multi_rounds[0]];
    let two_positions = breakpoint_positions(two_body);
    let b0 = two_positions[0].0;
    let b1 = two_positions[1].0;
    let two_input = two_body["input"].as_array().unwrap();
    let b0_text = item_text(&two_input[b0]);
    let b1_text = item_text(&two_input[b1]);
    assert_ne!(b0_text, b1_text, "B0 and B1 name different boundaries");
    for &round in &main_rounds {
        let positions = breakpoint_positions(&parsed[round]);
        let (some_breakpoint, _) = positions[0];
        let text = item_text(&parsed[round]["input"].as_array().unwrap()[some_breakpoint]);
        assert_eq!(
            text, b0_text,
            "the B0 content prefix is stable across rounds of the composed runtime"
        );
    }

    // ---- Maintenance lane: its own namespace, its own write policy ----
    let expected_maintenance_key =
        format!("{ISOLATION}|{workspace_identity}|{base_url}|compaction|maintenance");
    for &round in &compactor_rounds {
        let body = &parsed[round];
        let key = body["prompt_cache_key"]
            .as_str()
            .expect("the compactor request carries the composition root's maintenance key");
        assert_eq!(
            key, expected_maintenance_key,
            "the maintenance lane routes under the compaction|maintenance namespace"
        );
        assert!(
            !keys_a.contains(key) && !keys_b.contains(key),
            "the maintenance key differs from every main-lane key"
        );
        assert_eq!(
            body["prompt_cache_options"]["mode"].as_str(),
            Some("explicit"),
            "the compactor declares ExplicitOnly; with no reuse boundary the wire shape is explicit mode"
        );
        assert!(
            breakpoint_positions(body).is_empty(),
            "ExplicitOnly with no reuse boundary maps to ZERO breakpoints — a no-cache-write call, never an implicit suffix write"
        );
        assert!(
            body["tools"].as_array().unwrap().is_empty(),
            "compaction never sends tools"
        );
        assert_eq!(
            body["max_output_tokens"].as_u64(),
            Some(COMPACTION_OUTPUT_CHARS as u64),
            "the maintenance call carries its own output bound, not the main profile's"
        );
    }
}

#[tokio::test]
async fn empty_stable_set_first_request_declares_only_b0_in_the_declared_shape() {
    allow_loopback_direct();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_capture_server(Arc::clone(&bodies)).await;
    let base_url = format!("http://127.0.0.1:{port}/v1");

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let workspace_identity = opaque_workspace_identity(workspace.root());
    let routing = routing_for(
        "acceptance-empty",
        workspace_identity.clone(),
        base_url.clone(),
    );
    // Zero maintenance budget: no compactor attached, this composition only
    // exercises the main lane's first request.
    let zero_budget = MaintenanceBudget {
        max_calls_per_maintain: 0,
        max_tokens_per_maintain: 0,
        ..MaintenanceBudget::default()
    };
    let context_engine = build_context_engine(
        ContextPolicy::Dynamic,
        workspace.state_dir(),
        None,
        None,
        &zero_budget,
        None,
    )
    .await
    .unwrap();

    let config = ComposeConfig {
        provider_profile_digest: None,
        cache_routing: Some(routing),
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model: wire_provider(base_url.clone(), OpenAiPromptCacheMode::ResponsesExplicit),
        approval: Arc::new(PolicyApprovalGate::read_only()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: None,
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: None,
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    };
    let composed = compose(config).await.unwrap();
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();
    let mut events = composed.subscribe();
    handle
        .set_focus("empty-set first request".into())
        .await
        .unwrap();
    let task = wait_for_focus(&mut events, "empty-set").await;
    handle
        .user_message("the first turn against an empty stable set".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let _ = composed.shutdown().await;

    let captured = bodies.lock().unwrap();
    assert!(
        !captured.is_empty(),
        "the first main request must reach the wire"
    );
    let body: Value =
        serde_json::from_str(&captured[0]).expect("the captured body is responses JSON");

    let key = body["prompt_cache_key"]
        .as_str()
        .expect("the first request carries the routing key");
    assert_eq!(
        key,
        format!("acceptance-empty|{workspace_identity}|{base_url}|{task}|main"),
        "the very first request already uses the composed stable key"
    );
    assert_eq!(
        body["prompt_cache_options"]["mode"].as_str(),
        Some("explicit"),
        "explicit mode with the declared boundary — never a silent provider-default payload"
    );

    // An EMPTY stable set declares only B0, in the DECLARED sibling form.
    // The legacy `None` compatibility shape would instead rewrite the
    // boundary message's content into an array and hide the breakpoint
    // inside it (claiming the whole frame reusable) — a different wire.
    let positions = breakpoint_positions(&body);
    assert_eq!(
        positions,
        vec![(1, BreakpointForm::SiblingField)],
        "an empty stable set declares exactly B0 as a per-item sibling field: {positions:?}"
    );
    let input = body["input"].as_array().unwrap();
    assert!(
        input[1]["content"].is_string(),
        "the declared shape keeps the message content intact (no legacy content-array rewrite)"
    );
    assert!(input.len() > 2, "a volatile tail exists beyond B0");
    let mut turn_text_after_b0 = false;
    for item in input.iter().skip(2) {
        assert!(
            item_breakpoint_form(item).is_none(),
            "nothing beyond the stable policy may be declared cacheable"
        );
        if item_text(item).contains("empty stable set") {
            turn_text_after_b0 = true;
        }
    }
    assert!(
        turn_text_after_b0,
        "the first turn's changing text sits after the declared B0"
    );
}

#[tokio::test]
async fn unconfirmed_endpoint_strips_every_cache_field_from_the_wire() {
    allow_loopback_direct();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let port = spawn_capture_server(Arc::clone(&bodies)).await;
    let base_url = format!("http://127.0.0.1:{port}/v1");

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let workspace_identity = opaque_workspace_identity(workspace.root());
    // The routing namespace is configured — the runtime WILL stamp the
    // request — but the endpoint never confirmed the capability.
    let routing = routing_for("acceptance-default", workspace_identity, base_url.clone());
    let zero_budget = MaintenanceBudget {
        max_calls_per_maintain: 0,
        max_tokens_per_maintain: 0,
        ..MaintenanceBudget::default()
    };
    let context_engine = build_context_engine(
        ContextPolicy::Rolling,
        workspace.state_dir(),
        None,
        None,
        &zero_budget,
        None,
    )
    .await
    .unwrap();

    let config = ComposeConfig {
        provider_profile_digest: None,
        cache_routing: Some(routing),
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model: wire_provider(base_url.clone(), OpenAiPromptCacheMode::ProviderDefault),
        approval: Arc::new(PolicyApprovalGate::read_only()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: None,
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: None,
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    };
    let composed = compose(config).await.unwrap();
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();
    let mut events = composed.subscribe();
    handle
        .set_focus("unconfirmed endpoint check".into())
        .await
        .unwrap();
    handle
        .user_message("one turn against an unconfirmed endpoint".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let _ = composed.shutdown().await;

    let captured = bodies.lock().unwrap();
    assert!(!captured.is_empty(), "the turn must reach the wire");
    const CACHE_FIELDS: &[&str] = &[
        "prompt_cache_key",
        "prompt_cache_options",
        "prompt_cache_breakpoint",
    ];
    for (round, raw) in captured.iter().enumerate() {
        let body: Value = serde_json::from_str(raw).expect("the captured body is responses JSON");
        assert_eq!(
            body["model"].as_str(),
            Some("capture-model"),
            "round {round} is a real model request"
        );
        assert!(
            !carries_any_field(&body, CACHE_FIELDS),
            "an unconfirmed endpoint keeps the exact historical payload — no cache fields anywhere (round {round})"
        );
    }
}

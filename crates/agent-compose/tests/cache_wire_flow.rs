//! N04 (C1) acceptance: the STABLE cache routing key and the declared
//! B0/B1 breakpoints reach the REAL production wire — Compose builds the
//! services, the actor stamps the request at the single production call
//! site, and the real OpenAI Responses transport emits HTTP against a
//! local capture server. Two turns of the same task must carry the same
//! non-empty key, and both declared breakpoints must be visible on the
//! emitted input items (with the empty-message/expansion index mapping
//! the transport owns).
//!
//! This is a wire-level assertion, not a DTO hand-fill: nothing in the
//! test writes `prompt_cache_key` itself — only the production chain does.

use std::sync::Arc;
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::PromptCacheRouting;
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use provider_openai::{OpenAiConfig, OpenAiPromptCacheMode, OpenAiProtocol, OpenAiProvider};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A persistent capture server: one SSE Responses stream per POST, every
/// request body appended to the shared log.
async fn spawn_capture_server(bodies: Arc<std::sync::Mutex<Vec<String>>>) -> u16 {
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
                // Read to end of the request body (the transport sends and
                // then reads the response; half-close is not guaranteed, so
                // parse headers and read content-length).
                let mut read = vec![0u8; 16 * 1024];
                loop {
                    let Ok(n) = socket.read(&mut read).await else {
                        break;
                    };
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
                let round = bodies.lock().unwrap().len();
                bodies.lock().unwrap().push(body);

                // Round 1 of turn 1 issues a REAL workspace write, so turn 2
                // has a non-empty selection (the B0+B1 case); every other
                // round answers with plain text and completes.
                let sse_body = if round == 0 {
                    // `arguments` is the RAW JSON TEXT on the wire, not a
                    // nested object (the parser validates the string form).
                    let arguments = json!({"path": "evidence.txt",
                                           "content": "captured evidence for turn 2"})
                    .to_string();
                    let call = json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "function_call",
                            "call_id": "write-1",
                            "name": "fs.write",
                            "arguments": arguments,
                        }
                    });
                    let completed = json!({
                        "type": "response.completed",
                        "response": {"usage": {"input_tokens": 40, "output_tokens": 10}}
                    });
                    format!(
                        "event: response.output_item.done\r\ndata: {}\r\n\r\nevent: response.completed\r\ndata: {}\r\n\r\n",
                        call, completed
                    )
                } else {
                    let delta = json!({"type": "response.output_text.delta",
                                       "output_index": 0, "delta": "done"});
                    let completed = json!({"type": "response.completed",
                                           "response": {"usage": {"input_tokens": 50,
                                                                  "output_tokens": 5}}});
                    format!(
                        "event: response.output_text.delta\r\ndata: {}\r\n\r\nevent: response.completed\r\ndata: {}\r\n\r\n",
                        delta, completed
                    )
                };
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

fn responses_provider(base_url: String) -> Arc<dyn agent_contracts::ModelTransport> {
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
    // NO_PROXY for the loopback capture server (Windows system-proxy
    // environments otherwise intercept it; documented repo-wide).
    // Safety: single-threaded test-process env setup before any request.
    unsafe { std::env::set_var("NO_PROXY", "127.0.0.1,localhost") };
    // A no-proxy client: the loopback capture server must not be routed
    // through a system proxy (documented repo-wide Windows issue).
    let provider = OpenAiProvider::with_client(
        config,
        reqwest::Client::builder().no_proxy().build().unwrap(),
    )
    .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
    .expect("responses protocol supports the explicit cache mode");
    Arc::new(provider)
}

#[tokio::test]
async fn two_production_requests_of_one_task_share_the_key_and_carry_b0_b1() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = spawn_capture_server(Arc::clone(&bodies)).await;
    let base_url = format!("http://127.0.0.1:{port}/v1");

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let routing = PromptCacheRouting {
        isolation: "test-isolation".into(),
        workspace: workspace.root().display().to_string(),
        endpoint: base_url.clone(),
    };
    let maintenance_key = Some(routing.key_for("compaction", "maintenance"));

    let context_engine = agent_compose::build_context_engine(
        agent_compose::ContextPolicy::Dynamic,
        &workspace.state_dir(),
        None,
        None,
        &agent_compose::MaintenanceBudget::default(),
        maintenance_key,
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
        model: responses_provider(format!("{base_url}/responses-none")),
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
    // Subscribe BEFORE the first turn: an early TurnCompleted must not be
    // missed by a late subscriber.
    let mut events = composed.subscribe();

    // The production flow: set_focus creates the task while idle; the two
    // user messages are two turns of that same task.
    handle.set_focus("the capture task".into()).await.unwrap();
    handle
        .user_message("first turn of the capture task".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    handle
        .user_message("second turn of the capture task".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    let _ = composed.shutdown().await;

    let captured = bodies.lock().unwrap();
    eprintln!("captured HTTP rounds: {}", captured.len());
    assert!(
        captured.len() >= 3,
        "turn 1 (tool round + text round) and turn 2 must reach the wire, got {}",
        captured.len()
    );

    let parse = |body: &str| -> serde_json::Value {
        serde_json::from_str(body).expect("every captured body is the responses JSON")
    };
    let parsed: Vec<serde_json::Value> = captured.iter().map(|body| parse(body)).collect();

    // The routing key: identical across EVERY round of the task, non-empty,
    // composed from the configured isolation/workspace/endpoint/task/lane —
    // and never the request content itself.
    let keys: Vec<String> = parsed
        .iter()
        .map(|body| {
            body["prompt_cache_key"]
                .as_str()
                .expect("every request carries the key")
                .to_string()
        })
        .collect();
    assert!(
        keys.iter().all(|key| key == &keys[0]),
        "one task, one stable key across all rounds: {keys:?}"
    );
    assert!(keys[0].contains("test-isolation"));
    assert!(keys[0].contains("|main"));
    assert!(
        !keys[0].contains("capture task"),
        "the key is routing, not request content: {}",
        keys[0]
    );

    // The explicit cache capability is declared on the wire.
    assert_eq!(
        parsed[0]["prompt_cache_options"]["mode"].as_str(),
        Some("explicit")
    );

    // B0/B1 mapped per the DECLARED selection: round 1 runs with an empty
    // selection, so the honest wire carries only B0 (N05 — an empty stable
    // set is explicit, never the legacy whole-frame prefix). Once the
    // fs.write evidence exists, a later round carries BOTH B0 and B1, in
    // order, mapped through empty-message filtering and tool-call item
    // expansion.
    let breakpoint_positions = |body: &serde_json::Value| -> Vec<usize> {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let on_item = item.get("prompt_cache_breakpoint").is_some();
                let on_content = item["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| part.get("prompt_cache_breakpoint").is_some())
                });
                (on_item || on_content).then_some(index)
            })
            .collect()
    };
    let first_round = breakpoint_positions(&parsed[0]);
    assert_eq!(
        first_round,
        vec![1],
        "an empty selection declares only B0 (the stable policy end): {first_round:?}"
    );
    let with_selection = parsed
        .iter()
        .map(breakpoint_positions)
        .find(|positions| positions.len() == 2)
        .expect("a round with a non-empty selection must carry B0 and B1");
    assert!(
        with_selection[0] < with_selection[1],
        "B0 precedes B1: {with_selection:?}"
    );
}

async fn wait_for_turn_completed(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete within the test deadline"
        );
        match events.try_recv() {
            Ok(envelope) => {
                if matches!(envelope.event, agent_contracts::RuntimeEvent::TurnCompleted) {
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

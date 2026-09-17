//! KV production-assembly sequence acceptance: ONE task, ONE workspace, ONE
//! capture server — the whole production trajectory driven by the REAL
//! Compose/Actor/Provider/tool/Context stack (no hand-filled `ModelInput`):
//!
//! ```text
//! real reads -> new evidence in -> focus/steer change -> file version change
//!   -> tool load/withdraw -> checkpoint/restore (real teardown+recompose)
//!   -> multi-page log walk via the tool's returned continuation (dynamic
//!      round count: the read tool pages under the final model budget)
//!   -> failed call's usage settlement
//! ```
//!
//! Every step's final HTTP request is captured by the local loopback server
//! and compared with its predecessor: routing key, tool table and schema
//! stability, the stable-prefix boundary, effective body integrity
//! (required bodies really present — never trimmed away for cache
//! stability), and the known/unknown usage state of the ledger row.
//!
//! Disk truth is asserted where commentary is not enough: the scripted
//! `fs.write` of `evidence.txt` must land byte-exact on the workspace and
//! its bytes must re-enter a LATER request body through a real `fs.read`
//! (including after the checkpoint/restore). The existing
//! `cache_wire_flow` smoke and the provider-level `task_sequence` matrix
//! stay untouched; this test extends the journey, it does not replace
//! them.
//!
//! Tenth batch (C 续) adds three verification classes on top of the
//! ninth-batch assertions, which stay intact:
//!
//! 1. FULL STABLE PREFIX — every round's ENTIRE `input[0..=B]` (B0 against
//!    the trajectory baseline on every round; the declared B1 region within
//!    a turn) and the participating tools/schema blocks are compared
//!    item-for-item, with the first divergent item index named on failure.
//!    The ninth-batch checks compared `input[B0]` plus sampled adjacent
//!    first-differences; items before the breakpoint were a blind spot.
//! 2. REAL DELIVERY EVIDENCE — a pre-seeded multi-page log is walked by the
//!    scripted model EXACTLY along the tool's OWN returned continuation
//!    (the product rule under test: use the returned clause verbatim, walk
//!    to true EOF). The read tool pages under the FINAL model-content
//!    budget, so each page's delivered body must contain its whole claimed
//!    range verbatim — no truncation marker, no head+tail clip — and every
//!    block ID must be delivered exactly once, on the page claiming to
//!    cover its line, with its true source line number. The union of the
//!    delivered claims must cover the file with no gaps and no overlaps.
//! 3. FULL COST LEDGER — every ledger row records the provider cache
//!    read/write/miss buckets with an EXPLICIT unknown marker when the
//!    provider did not report them, the real attempt count, the call lane
//!    (main vs maintenance), and the ledger totals must equal the sum of
//!    the per-row values (a cumulative usage snapshot cannot be counted
//!    twice).
//!
//! Honesty boundary: LOCAL_WIRE only. The scripted usage numbers prove
//! TRANSPORT and SETTLEMENT only — they say nothing about real cache hit
//! rates or price savings. Endpoint acceptance, real server-side cache
//! hits and net task cost are NOT_RUN here (no paid endpoint is contacted;
//! the server is a 127.0.0.1 random-port capture).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_compose::{ComposeConfig, MaintenanceBudget, compose};
use agent_contracts::{ModelCallRole, PromptCacheRouting, RuntimeEvent, UsageIdentity};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use provider_openai::{
    DEFAULT_MAX_STREAM_BYTES, OpenAiConfig, OpenAiPromptCacheMode, OpenAiProtocol, OpenAiProvider,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// The scripted model: a persistent capture server answers each POST with the
// next scripted decision (tool call, plain completion, or a billed terminal
// failure). `input_tokens` is unique per round so every ledger row can be
// matched to exactly one wire request. The big-log walk segment is
// DYNAMIC: once the fixed pre-segment is consumed, the next decision is
// derived from the tool's returned continuation clause in the request the
// server just captured — the product rule under test, applied by the
// script itself.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Script {
    /// One tool call; the round completes with usage.
    Call {
        call_id: String,
        name: &'static str,
        arguments: Value,
        input_tokens: u64,
    },
    /// A plain text completion with usage.
    Text {
        delta: &'static str,
        input_tokens: u64,
    },
    /// A billed terminal failure: `response.failed` reports usage without
    /// completing the response; the error code is non-retryable.
    Fail {
        code: &'static str,
        message: &'static str,
        input_tokens: u64,
        output_tokens: u64,
    },
}

impl Script {
    fn usage(&self) -> (u64, u64) {
        match self {
            Script::Call { input_tokens, .. } | Script::Text { input_tokens, .. } => {
                (*input_tokens, 8)
            }
            Script::Fail {
                input_tokens,
                output_tokens,
                ..
            } => (*input_tokens, *output_tokens),
        }
    }
}

#[derive(Default)]
struct WalkDrive {
    /// How many continuation pages the walk segment has issued.
    continuations_served: usize,
    /// Next unique `input_tokens` value for walk-generated rounds.
    next_token: u64,
    /// Whether the walk's completion round was already served.
    completed: bool,
}

struct ServerState {
    bodies: Mutex<Vec<String>>,
    /// Fixed decisions before the dynamic walk (turns 1-6 plus the walk's
    /// scripted opener request).
    pre_scripts: Mutex<VecDeque<Script>>,
    /// Fixed decisions after the walk (the billed terminal failure).
    post_scripts: Mutex<VecDeque<Script>>,
    /// The dynamic big-log walk state.
    walk: Mutex<WalkDrive>,
    /// Every decision actually served, in order — the ledger's ground
    /// truth (the walk segment is generated, not predeclared).
    served: Mutex<Vec<Script>>,
    unexpected_rounds: AtomicUsize,
}

/// The tool's returned continuation clause for the big-log walk, parsed
/// from a delivered fs.read body (`[coverage] ...; continue with fs.read
/// path=logs/big.log start_line=N end_line=M`). `None` = the tool offered
/// no continuation (true EOF for a full walk, or a contract break the
/// caller must classify).
fn walk_continuation_clause(body_text: &str) -> Option<(u64, u64)> {
    const CLAUSE_HEAD: &str = "continue with fs.read path=logs/big.log start_line=";
    const END_MARK: &str = " end_line=";
    let at = body_text.find(CLAUSE_HEAD)? + CLAUSE_HEAD.len();
    let rest = &body_text[at..];
    let start: u64 = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let end_at = rest.find(END_MARK)? + END_MARK.len();
    let end: u64 = rest[end_at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((start, end))
}

/// The next decision for a captured request once the fixed pre-segment is
/// consumed: follow the tool's returned continuation until true EOF, then
/// serve the walk's completion once. `None` = no decidable scripted round
/// (the caller counts the round as unexpected).
fn next_walk_script(state: &ServerState, wire: &Value) -> Option<Script> {
    let claims = delivered_big_log_pages(wire);
    let mut drive = state.walk.lock().unwrap();
    if drive.completed {
        return None;
    }
    match claims.last() {
        // No big-log result to continue from: undecidable.
        None => None,
        // True EOF: the last delivered claim reaches the file's final line.
        Some((_, end, _)) if *end >= BIG_LOG_LINES as u64 => {
            let token = drive.next_token;
            drive.next_token += 1;
            drive.completed = true;
            Some(Script::Text {
                delta: "t6b complete",
                input_tokens: token,
            })
        }
        // More remains: the page MUST carry its continuation clause; the
        // next decision is that clause verbatim.
        Some((_, _, text)) => match walk_continuation_clause(text) {
            Some((start, end)) => {
                let token = drive.next_token;
                drive.next_token += 1;
                drive.continuations_served += 1;
                Some(Script::Call {
                    call_id: format!("biglog-cont-{}", drive.continuations_served),
                    name: "fs.read",
                    arguments: json!({
                        "path": BIG_LOG_PATH,
                        "start_line": start,
                        "end_line": end,
                    }),
                    input_tokens: token,
                })
            }
            // A non-EOF page without a continuation clause breaks the
            // product rule; serving nothing makes the round unexpected and
            // the coverage assertions below name the defect.
            None => None,
        },
    }
}

async fn spawn_sequence_server(state: Arc<ServerState>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let body = match read_request_body(&mut socket).await {
                    Some(body) => body,
                    None => return,
                };
                let wire: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                {
                    state.bodies.lock().unwrap().push(body);
                }
                let script = match state.pre_scripts.lock().unwrap().pop_front() {
                    Some(script) => Some(script),
                    None => {
                        // The fixed post-segment (the billed failure) may
                        // only run AFTER the walk completed; an undecidable
                        // mid-walk round is an unexpected round, never the
                        // post segment.
                        let walk_completed = state.walk.lock().unwrap().completed;
                        if walk_completed {
                            next_walk_script(&state, &wire)
                                .or_else(|| state.post_scripts.lock().unwrap().pop_front())
                        } else {
                            next_walk_script(&state, &wire)
                        }
                    }
                };
                let script = match script {
                    Some(script) => script,
                    None => {
                        // A round the trajectory did not script (an
                        // unexpected retry or an extra model call): serve a
                        // harmless completion, count it, and let the final
                        // assertions fail on the count.
                        state.unexpected_rounds.fetch_add(1, Ordering::SeqCst);
                        Script::Text {
                            delta: "unexpected-round",
                            input_tokens: 1,
                        }
                    }
                };
                state.served.lock().unwrap().push(script.clone());
                let (input_tokens, output_tokens) = script.usage();
                let sse_body = match script {
                    Script::Call {
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        // `arguments` is the RAW JSON TEXT on the wire.
                        let call = json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments.to_string(),
                            }
                        });
                        let completed = json!({
                            "type": "response.completed",
                            "response": {"usage": {
                                "input_tokens": input_tokens,
                                "output_tokens": output_tokens,
                            }}
                        });
                        format!(
                            "event: response.output_item.done\r\ndata: {call}\r\n\r\n\
                             event: response.completed\r\ndata: {completed}\r\n\r\n"
                        )
                    }
                    Script::Text { delta, .. } => {
                        let delta = json!({"type": "response.output_text.delta",
                                           "output_index": 0, "delta": delta});
                        let completed = json!({
                            "type": "response.completed",
                            "response": {"usage": {
                                "input_tokens": input_tokens,
                                "output_tokens": output_tokens,
                            }}
                        });
                        format!(
                            "event: response.output_text.delta\r\ndata: {delta}\r\n\r\n\
                             event: response.completed\r\ndata: {completed}\r\n\r\n"
                        )
                    }
                    Script::Fail {
                        code,
                        message,
                        input_tokens,
                        output_tokens,
                    } => {
                        let delta = json!({"type": "response.output_text.delta",
                                           "output_index": 0, "delta": "partial"});
                        let failed = json!({
                            "type": "response.failed",
                            "response": {
                                "error": {"code": code, "message": message},
                                "usage": {
                                    "input_tokens": input_tokens,
                                    "output_tokens": output_tokens,
                                },
                            }
                        });
                        format!(
                            "event: response.output_text.delta\r\ndata: {delta}\r\n\r\n\
                             event: response.failed\r\ndata: {failed}\r\n\r\n"
                        )
                    }
                };
                let sse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse_body}",
                    sse_body.len()
                );
                let _ = socket.write_all(sse.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    addr.port()
}

async fn read_request_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 16 * 1024];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let length: usize = String::from_utf8_lossy(&bytes[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0u8; 16 * 1024];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&bytes[header_end..]).to_string())
}

// ---------------------------------------------------------------------------
// Production composition: real Compose/Actor/Provider over the capture
// server, real builtin tools, permissive approval (the trajectory performs
// real writes and asserts their disk bytes), durable Dynamic context engine
// with no compactor attached (so no maintenance call ever reaches the wire
// and every captured round is a main-lane model round).
// ---------------------------------------------------------------------------

async fn production_compose(
    root: &std::path::Path,
    base_url: &str,
    routing: &PromptCacheRouting,
) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = Workspace::open(root).await?;
    let context_engine = agent_compose::build_context_engine(
        agent_compose::ContextPolicy::Dynamic,
        workspace.state_dir(),
        None,
        None,
        &MaintenanceBudget::default(),
        None,
    )
    .await?;
    let config = ComposeConfig {
        provider_profile_digest: None,
        cache_routing: Some(routing.clone()),
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model: responses_provider(base_url.to_string()),
        approval: Arc::new(PolicyApprovalGate::permissive()),
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
    compose(config).await
}

fn responses_provider(base_url: String) -> Arc<dyn agent_contracts::ModelTransport> {
    let config = OpenAiConfig {
        api_key: "test-key".into(),
        base_url,
        model: "capture-model".into(),
        protocol: OpenAiProtocol::Responses,
        max_output_tokens: 256,
        timeout: Duration::from_secs(20),
        send_stream_options: false,
        send_max_tokens: true,
        max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
        context_window: Some(64_000),
        sampling: provider_openai::SamplingPolicy::ProviderDefault,
    };
    // NO_PROXY for the loopback capture server (Windows system-proxy
    // environments otherwise intercept it; documented repo-wide).
    // Safety: single-threaded test-process env setup before any request.
    unsafe { std::env::set_var("NO_PROXY", "127.0.0.1,localhost") };
    let provider = OpenAiProvider::with_client(
        config,
        reqwest::Client::builder().no_proxy().build().unwrap(),
    )
    .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
    .expect("responses protocol supports the explicit cache mode");
    Arc::new(provider)
}

// ---------------------------------------------------------------------------
// Event helpers.
// ---------------------------------------------------------------------------

/// One provider-reported token counter, or the EXPLICIT unknown marker for
/// a counter the provider did not report. An unreported counter is never
/// an invented zero and is never summed as one (COST-6 discipline,
/// mirrored test-side): the local scripted transport proves transport and
/// settlement only, so its absent cache buckets must stay visibly absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CostCounter {
    Known(u64),
    Unknown,
}

/// The ledger facts of one main-lane model round. The ninth batch recorded
/// input/output/identity; the tenth batch (C 续) extends every row with the
/// provider cache read/write/miss buckets, the real attempt count, the
/// call lane, and whether a typed usage report arrived at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LedgerRow {
    input_tokens: u64,
    output_tokens: u64,
    identity: UsageIdentity,
    /// Provider-reported prompt-cache READ tokens for this round
    /// (`input_tokens_details.cached_tokens`), when reported.
    cached_input_tokens: CostCounter,
    /// Provider-reported cache WRITE tokens (explicit write counter only).
    cache_write_input_tokens: CostCounter,
    /// Provider-reported cache MISS tokens (read-side observation).
    cache_miss_input_tokens: CostCounter,
    /// The legacy event-level flattened cache-read counter (0 when the
    /// provider did not report one) — cross-checked against the typed
    /// bucket so a flatten bug cannot hide an unreported bucket.
    event_cached_input_tokens: u64,
    /// Real transport attempts that produced this round.
    attempts: u32,
    retries: u32,
    /// Which call lane produced this row (main vs maintenance).
    role: ModelCallRole,
    /// Whether the typed per-field `ModelUsage` report arrived at all.
    typed_usage_reported: bool,
}

/// Collects every `ModelUsed` row of one runtime session on its OWN
/// subscription, so the assertion helpers (which consume a different
/// receiver) can never drop ledger evidence.
fn spawn_ledger_collector(
    mut events: tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    rows: Arc<Mutex<Vec<LedgerRow>>>,
    lags: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.try_recv() {
                Ok(envelope) => {
                    if let RuntimeEvent::ModelUsed {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        attempts,
                        retries,
                        usage_identity,
                        role,
                        usage,
                        ..
                    } = envelope.event
                    {
                        let typed = usage.as_ref();
                        let typed_counter = |field: &Option<u64>| {
                            field
                                .map(CostCounter::Known)
                                .unwrap_or(CostCounter::Unknown)
                        };
                        rows.lock().unwrap().push(LedgerRow {
                            input_tokens,
                            output_tokens,
                            identity: usage_identity,
                            cached_input_tokens: typed_counter(
                                &typed.map(|usage| usage.cached_input_tokens).unwrap_or(None),
                            ),
                            cache_write_input_tokens: typed_counter(
                                &typed
                                    .map(|usage| usage.cache_write_input_tokens)
                                    .unwrap_or(None),
                            ),
                            cache_miss_input_tokens: typed_counter(
                                &typed
                                    .map(|usage| usage.cache_miss_input_tokens)
                                    .unwrap_or(None),
                            ),
                            event_cached_input_tokens: cached_input_tokens,
                            attempts,
                            retries,
                            role,
                            typed_usage_reported: usage.is_some(),
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    lags.fetch_add(1, Ordering::SeqCst);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }
    })
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    mut matches: impl FnMut(&RuntimeEvent) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never {what} within the test deadline"
        );
        match events.try_recv() {
            Ok(envelope) => {
                if matches(&envelope.event) {
                    return;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("the event stream closed before {what}")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_turn_completed(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) {
    wait_for(
        events,
        |event| matches!(event, RuntimeEvent::TurnCompleted),
        "complete a turn",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Wire analysis helpers.
// ---------------------------------------------------------------------------

/// Indexes of the wire input items that carry an explicit cache breakpoint
/// (on the item or on one of its content blocks — the transport owns the
/// empty-message/expansion mapping).
fn breakpoint_positions(body: &Value) -> Vec<usize> {
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
}

/// Index of the first differing wire input item, or `None` when identical.
fn first_input_diff(previous: &Value, current: &Value) -> Option<usize> {
    let previous_items = previous["input"].as_array().unwrap();
    let current_items = current["input"].as_array().unwrap();
    let shared = previous_items
        .iter()
        .zip(current_items)
        .position(|(left, right)| left != right);
    match (shared, previous_items.len() == current_items.len()) {
        (Some(index), _) => Some(index),
        (None, true) => None,
        (None, false) => Some(previous_items.len().min(current_items.len())),
    }
}

fn tool_names(wire: &Value) -> Vec<String> {
    let mut names = wire["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// The text of a mapped input item (the breakpoint block form carries the
/// message body in `content[0].text`); falls back to the raw item JSON.
fn item_text(item: &Value) -> String {
    item["content"][0]["text"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| item.to_string())
}

fn assert_sentinels(haystack: &str, present: &[&str], round: usize) {
    for sentinel in present {
        assert!(
            haystack.contains(sentinel),
            "round {round}: required body sentinel {sentinel} must really be on the wire"
        );
    }
}

// ---------------------------------------------------------------------------
// Tenth-batch helpers: FULL prefix comparison and delivered-body extraction.
// ---------------------------------------------------------------------------

/// First index inside `input[0..=end]` at which two requests differ, or
/// `None` when the whole compared prefix is item-for-item identical. The
/// failure paths below must name THIS index — "somewhere in the prefix
/// differs" is not an actionable stable-boundary fact.
fn first_prefix_divergence(previous: &Value, current: &Value, end: usize) -> Option<usize> {
    previous["input"]
        .as_array()
        .unwrap()
        .iter()
        .zip(current["input"].as_array().unwrap())
        .take(end + 1)
        .position(|(left, right)| left != right)
}

/// A bounded single-line preview of one input item for failure output.
fn item_preview(item: &Value) -> String {
    let text = item_text(item).replace(['\n', '\r'], "\\n");
    text.chars().take(110).collect::<String>()
}

/// The participating tool table as one name-keyed canonical block, so two
/// rounds offering the SAME tool set can be compared as a whole
/// (order-independent; each named schema is compared byte-for-byte).
fn canonical_tools(wire: &Value) -> std::collections::BTreeMap<String, Value> {
    wire["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_string(), tool.clone()))
        .collect()
}

/// The wire input items of a captured request.
fn wire_input_items(wire: &Value) -> &[Value] {
    wire["input"].as_array().unwrap()
}

/// The delivered text of a `function_call_output` item (`output` is the
/// plain string, or `input_text` blocks when a breakpoint was placed on
/// it).
fn function_call_output_text(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
        return None;
    }
    Some(match &item["output"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| block["text"].as_str().unwrap_or(""))
            .collect::<String>(),
        other => other.to_string(),
    })
}

/// The `lines=S-E/TOTAL` coverage claim of an fs.read result body, when
/// parseable.
fn parse_claimed_lines(text: &str) -> Option<(u64, u64, u64)> {
    let at = text.find("lines=")? + "lines=".len();
    let segment: String = text[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '/')
        .collect();
    let mut parts = segment.split('/');
    let mut range = parts.next()?.split('-');
    let start = range.next()?.parse().ok()?;
    let end = range.next()?.parse().ok()?;
    let total = parts.next()?.parse().ok()?;
    Some((start, end, total))
}

// ---------------------------------------------------------------------------
// The trajectory.
// ---------------------------------------------------------------------------

/// Body bytes of the two pre-seeded workspace files and the scripted
/// `evidence.txt` artifact.
const ALPHA_REV1: &str = "ALPHA-REV1-SENTINEL original inventory row\n";
const ALPHA_REV2: &str = "ALPHA-REV2-SENTINEL rewritten inventory row\n";
const BETA_BODY: &str = "BETA-NEW-EVIDENCE-SENTINEL later retrieval row\n";
const EVIDENCE_V1: &str = "EVIDENCE-V1-ARCHIVE-BYTES";

// ---------------------------------------------------------------------------
// The pre-seeded multi-page log for the REAL delivery evidence (tenth
// batch, class 2). 600 lines of exactly 150 content chars. The scripted
// model opens with one 200-line window request and then follows the
// tool's returned continuation clauses verbatim; the read tool pages
// under the FINAL model-content budget (~98 lines per page for this line
// width), so the walk takes ~9 pages of DYNAMIC round count and every
// delivered page must be verbatim-complete.
// ---------------------------------------------------------------------------

const BIG_LOG_PATH: &str = "logs/big.log";
const BIG_LOG_LINES: usize = 600;
/// The scripted opener's window (the model's free first request); pages
/// two and beyond are the tool's own returned continuation clauses.
const BIG_LOG_OPENER_WINDOW: (u64, u64) = (1, 200);
const BIG_LOG_LINE_CHARS: usize = 150;

/// Unique block IDs at the review G2 counterexample positions.
const BIG_LOG_MID_MARKER_LINES: [(usize, &str); 3] = [
    (100, "KVSEQ-BIG-MID-P1-L100-9A71C3"),
    (300, "KVSEQ-BIG-MID-P2-L300-3C68E0"),
    (500, "KVSEQ-BIG-MID-P3-L500-E0B492"),
];

/// Head/tail controls: present whenever the page's head/tail regions are
/// delivered at all.
const BIG_LOG_CONTROL_MARKER_LINES: [(usize, &str); 2] = [
    (10, "KVSEQ-BIG-HEAD-CTRL-L10-4D2F11"),
    (590, "KVSEQ-BIG-TAIL-CTRL-L590-77AD06"),
];

/// One log line: exactly `BIG_LOG_LINE_CHARS` content chars, with the
/// block ID embedded at its marker line.
fn big_log_line(line: usize) -> String {
    let marker = BIG_LOG_MID_MARKER_LINES
        .iter()
        .chain(BIG_LOG_CONTROL_MARKER_LINES.iter())
        .find(|(marker_line, _)| *marker_line == line)
        .map(|(_, id)| *id);
    let mut base = match marker {
        Some(id) => format!("row-{line:04} {id} payload"),
        None => format!("row-{line:04} routine biglog filler payload"),
    };
    while base.chars().count() < BIG_LOG_LINE_CHARS {
        base.push('.');
    }
    base
}

/// Pre-seeds the workspace log and asserts the DISK truth the delivery
/// walk is judged against: every block ID really is in the file, at its
/// line, at the declared uniform line width.
fn seed_big_log(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("logs")).unwrap();
    let mut body = String::new();
    for line in 1..=BIG_LOG_LINES {
        body.push_str(&big_log_line(line));
        body.push('\n');
    }
    std::fs::write(root.join(BIG_LOG_PATH), body).unwrap();

    let disk = std::fs::read_to_string(root.join(BIG_LOG_PATH)).unwrap();
    let disk_lines: Vec<&str> = disk.lines().collect();
    assert_eq!(disk_lines.len(), BIG_LOG_LINES, "the seeded log line count");
    assert!(
        disk_lines
            .iter()
            .all(|line| line.chars().count() == BIG_LOG_LINE_CHARS),
        "the seeded log lines must be uniformly {BIG_LOG_LINE_CHARS} chars"
    );
    for (line, id) in BIG_LOG_MID_MARKER_LINES
        .iter()
        .chain(BIG_LOG_CONTROL_MARKER_LINES.iter())
    {
        assert!(
            disk_lines[line - 1].contains(id),
            "disk truth: block ID {id} must sit on line {line} of the seeded log"
        );
    }
}

/// The (start, end) page windows requested for the big log across one
/// captured request's `function_call` items.
fn requested_big_log_pages(wire: &Value) -> Vec<(u64, u64)> {
    wire_input_items(wire)
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item["name"].as_str() == Some("fs_read")
        })
        .filter_map(|item| serde_json::from_str::<Value>(item["arguments"].as_str()?).ok())
        .filter(|arguments| arguments["path"].as_str() == Some(BIG_LOG_PATH))
        .map(|arguments| {
            (
                arguments["start_line"].as_u64().unwrap_or(1),
                arguments["end_line"].as_u64().unwrap_or(1),
            )
        })
        .collect()
}

/// The big-log page bodies DELIVERED in one captured request, as
/// (claimed start, claimed end, delivered text).
fn delivered_big_log_pages(wire: &Value) -> Vec<(u64, u64, String)> {
    wire_input_items(wire)
        .iter()
        .filter_map(function_call_output_text)
        .filter(|text| text.contains(&format!("file=\"{BIG_LOG_PATH}\"")))
        .filter_map(|text| parse_claimed_lines(&text).map(|(start, end, _)| (start, end, text)))
        .collect()
}

/// The scripted trajectory's FIXED turn starts as 1-based round numbers:
/// T1:R1-3, T2:R4-5, T3:R6, T4:R7-9, T5:R10-12, T6:R13-14, T6b:R15..(the
/// dynamic walk's completion round), T7:R-last. A turn commit may
/// legitimately re-render the declared evidence item; mid-turn rounds may
/// not (the full-prefix checks key off this).
fn turn_start_rounds(total_rounds: usize) -> Vec<usize> {
    vec![1, 4, 6, 7, 10, 13, 15, total_rounds]
}

#[tokio::test]
async fn production_trajectory_of_one_task_over_the_local_capture_server() {
    // The FIXED pre-segment: turns 1-6 plus the walk's scripted opener.
    // The walk's continuation rounds are GENERATED by the server from the
    // tool's returned clauses (`next_walk_script`), so the round count is
    // dynamic — exactly what the product rule under test produces.
    let pre_scripts = vec![
        // Turn 1 ("T1"): real read, real write, complete.
        Script::Call {
            call_id: "r1-read-alpha".into(),
            name: "fs.read",
            arguments: json!({"path": "notes/alpha.txt"}),
            input_tokens: 1001,
        },
        Script::Call {
            call_id: "r2-write-evidence".into(),
            name: "fs.write",
            arguments: json!({"path": "evidence.txt", "content": EVIDENCE_V1}),
            input_tokens: 1002,
        },
        Script::Text {
            delta: "t1 complete",
            input_tokens: 1003,
        },
        // Turn 2 ("T2"): new evidence enters.
        Script::Call {
            call_id: "r4-read-beta".into(),
            name: "fs.read",
            arguments: json!({"path": "notes/beta.txt"}),
            input_tokens: 1004,
        },
        Script::Text {
            delta: "t2 complete",
            input_tokens: 1005,
        },
        // Turn 3: the operator steers the active task — the focus/directive
        // changes in-task.
        Script::Text {
            delta: "t3 acknowledged",
            input_tokens: 1006,
        },
        // Turn 4 ("T4"): the file changes version — rewrite then re-read.
        Script::Call {
            call_id: "r7-write-alpha".into(),
            name: "fs.write",
            arguments: json!({"path": "notes/alpha.txt", "content": ALPHA_REV2}),
            input_tokens: 1007,
        },
        Script::Call {
            call_id: "r8-read-alpha".into(),
            name: "fs.read",
            arguments: json!({"path": "notes/alpha.txt"}),
            input_tokens: 1008,
        },
        Script::Text {
            delta: "t4 complete",
            input_tokens: 1009,
        },
        // Turn 5 ("T5"): tool surface exercised through the model's own
        // catalog control — load a catalog-cold tool, then withdraw it.
        Script::Call {
            call_id: "r10-load-mkdir".into(),
            name: "capability.manage",
            arguments: json!({"op": "load", "name": "fs.mkdir"}),
            input_tokens: 1010,
        },
        Script::Call {
            call_id: "r11-unload-mkdir".into(),
            name: "capability.manage",
            arguments: json!({"op": "unload", "name": "fs.mkdir"}),
            input_tokens: 1011,
        },
        Script::Text {
            delta: "t5 complete",
            input_tokens: 1012,
        },
        // Turn 6: AFTER the checkpoint/restore — read the artifact the
        // earlier write produced; its bytes must re-enter the wire body.
        Script::Call {
            call_id: "r13-read-evidence".into(),
            name: "fs.read",
            arguments: json!({"path": "evidence.txt"}),
            input_tokens: 1013,
        },
        Script::Text {
            delta: "t6 complete",
            input_tokens: 1014,
        },
        // Turn 6b ("T6b", tenth batch): the walk's scripted OPENER — the
        // model's free first request. Every following page must equal the
        // tool's returned continuation clause, verbatim.
        Script::Call {
            call_id: "r15-read-biglog-opener".into(),
            name: "fs.read",
            arguments: json!({
                "path": BIG_LOG_PATH,
                "start_line": BIG_LOG_OPENER_WINDOW.0,
                "end_line": BIG_LOG_OPENER_WINDOW.1,
            }),
            input_tokens: 1016,
        },
    ];
    // The FIXED post-segment: after the walk completes, a billed terminal
    // failure — the known usage must settle exactly once and the
    // trajectory must end without any unscripted extra round.
    let post_scripts = vec![Script::Fail {
        code: "invalid_request_error",
        message: "scripted terminal failure",
        input_tokens: 1015,
        output_tokens: 44,
    }];

    let state = Arc::new(ServerState {
        bodies: Mutex::new(Vec::new()),
        pre_scripts: Mutex::new(VecDeque::from(pre_scripts.clone())),
        post_scripts: Mutex::new(VecDeque::from(post_scripts.clone())),
        walk: Mutex::new(WalkDrive {
            continuations_served: 0,
            // Continuation pages and the walk completion draw from here;
            // unique per round by construction.
            next_token: 1017,
            completed: false,
        }),
        served: Mutex::new(Vec::new()),
        unexpected_rounds: AtomicUsize::new(0),
    });
    let port = spawn_sequence_server(Arc::clone(&state)).await;
    let base_url = format!("http://127.0.0.1:{port}/v1");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir(root.join("notes")).unwrap();
    std::fs::write(root.join("notes/alpha.txt"), ALPHA_REV1).unwrap();
    std::fs::write(root.join("notes/beta.txt"), BETA_BODY).unwrap();
    // Tenth batch, class 2: pre-seed the multi-page log (disk truth is
    // asserted inside).
    seed_big_log(&root);

    let routing = PromptCacheRouting {
        isolation: "kv-sequence-isolation".into(),
        workspace: root.display().to_string(),
        endpoint: base_url.clone(),
    };

    // The ledger spans BOTH runtime sessions (the checkpoint/restore
    // restarts the runtime, not the trajectory).
    let ledger: Arc<Mutex<Vec<LedgerRow>>> = Arc::new(Mutex::new(Vec::new()));
    let ledger_lags = Arc::new(AtomicUsize::new(0));

    // ---- Session 1: turns 1-5. ----
    let composed = production_compose(&root, &base_url, &routing)
        .await
        .unwrap();
    let mut events = composed.subscribe();
    let collector = spawn_ledger_collector(
        composed.subscribe(),
        Arc::clone(&ledger),
        Arc::clone(&ledger_lags),
    );
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();

    handle
        .set_focus("T1 read alpha then archive the finding".into())
        .await
        .unwrap();
    let mut task_id = None;
    wait_for(
        &mut events,
        |event| {
            if let RuntimeEvent::FocusChanged { task_id: id, goal } = event
                && goal.contains("T1 read alpha")
            {
                task_id = Some(*id);
                return true;
            }
            false
        },
        "report FocusChanged(T1)",
    )
    .await;
    let task_id = task_id.expect("the focus event carries the task id");

    // Turn 1: read alpha, write evidence.txt, complete.
    handle
        .user_message("T1 report the alpha row and archive it".into())
        .await
        .unwrap();
    wait_turn_completed(&mut events).await;
    // DISK TRUTH: the write really landed, byte-exact — commentary about
    // "having sent fs.write" proves nothing.
    assert_eq!(
        std::fs::read_to_string(root.join("evidence.txt")).unwrap(),
        EVIDENCE_V1,
        "the scripted fs.write must have really produced evidence.txt"
    );

    // Turn 2: new evidence (beta) enters.
    handle
        .user_message("T2 also take in the beta row".into())
        .await
        .unwrap();
    wait_turn_completed(&mut events).await;

    // Turn 3: in-task steering — the focus/directive changes.
    let steer = handle
        .steer_active_task("T3 archive focus now".into(), Some(task_id))
        .await
        .unwrap();
    assert!(
        matches!(steer, agent_runtime::SteeringOutcome::Accepted { .. }),
        "the steering must be accepted on the active task: {steer:?}"
    );
    wait_turn_completed(&mut events).await;

    // Turn 4: file version change — rewrite alpha, then re-read it.
    handle
        .user_message("T4 refresh the alpha row".into())
        .await
        .unwrap();
    wait_turn_completed(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(root.join("notes/alpha.txt")).unwrap(),
        ALPHA_REV2,
        "the file version change must have really landed on disk"
    );

    // Turn 5: tool load + withdrawal through the model's own catalog
    // control plane.
    handle
        .user_message("T5 try the mkdir capability then withdraw it".into())
        .await
        .unwrap();
    wait_turn_completed(&mut events).await;

    // ---- Checkpoint / restore across a REAL teardown. ----
    let checkpoint = composed.instance.checkpoint().await.unwrap();
    checkpoint.validate().unwrap();
    let checkpoint_bytes = serde_json::to_vec(&checkpoint).unwrap();
    // The handle owns a broadcast sender: it must be dropped before the
    // collector can observe the channel close.
    drop(handle);
    composed.shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(30), collector).await;

    let composed = production_compose(&root, &base_url, &routing)
        .await
        .unwrap();
    let mut events = composed.subscribe();
    let collector = spawn_ledger_collector(
        composed.subscribe(),
        Arc::clone(&ledger),
        Arc::clone(&ledger_lags),
    );
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();
    let checkpoint: agent_runtime::RuntimeCheckpoint =
        serde_json::from_slice(&checkpoint_bytes).unwrap();
    composed.instance.restore(checkpoint).await.unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;
    let continued = handle.continue_active_task().await.unwrap();
    assert_eq!(
        continued, task_id,
        "the restored task keeps its identity — and therefore its routing key"
    );
    wait_turn_completed(&mut events).await;

    // Turn 6b: the multi-page log walk through the REAL tool — the scripted
    // model reads page by page, each page request following the previous
    // read's returned continuation (asserted from the wire at the end).
    handle
        .user_message("T6b walk the big log page by page with the returned continuation".into())
        .await
        .unwrap();
    wait_turn_completed(&mut events).await;

    // Turn 7: billed terminal failure.
    handle
        .user_message("T7 this call will fail".into())
        .await
        .unwrap();
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::Failure { .. }),
        "report the scripted failure",
    )
    .await;
    // The handle owns a broadcast sender: drop it (and the last receiver)
    // so the collector can observe the channel close.
    drop(handle);
    drop(events);
    composed.shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(30), collector).await;

    // ------------------------------------------------------------------
    // Every scripted-or-generated round reached the wire exactly once;
    // nothing extra.
    // ------------------------------------------------------------------
    let bodies = state.bodies.lock().unwrap().clone();
    let served = state.served.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        served.len(),
        "one served decision per captured round (got {} bodies, {} served; unexpected extra rounds: {})",
        bodies.len(),
        served.len(),
        state.unexpected_rounds.load(Ordering::SeqCst)
    );
    assert_eq!(state.unexpected_rounds.load(Ordering::SeqCst), 0);
    assert_eq!(
        ledger_lags.load(Ordering::SeqCst),
        0,
        "the ledger collector must never lag (rows would be lost)"
    );
    let wires: Vec<Value> = bodies
        .iter()
        .map(|body| serde_json::from_str(body).expect("every captured body is the responses JSON"))
        .collect();

    // ---- Dimension 1: the routing key — ONE task, ONE stable key across
    // every round, including across the restore. ----
    let keys: Vec<String> = wires
        .iter()
        .map(|wire| {
            wire["prompt_cache_key"]
                .as_str()
                .expect("the key is on every request")
                .to_string()
        })
        .collect();
    assert!(
        keys.iter().all(|key| key == &keys[0]),
        "one task, one stable key across the whole trajectory: {keys:?}"
    );
    assert_eq!(
        keys[0],
        routing.key_for(&task_id.to_string(), "main"),
        "the wire key is the composed routing digest of this task"
    );
    for (round, wire) in wires.iter().enumerate() {
        assert_eq!(
            wire["prompt_cache_options"]["mode"].as_str(),
            Some("explicit"),
            "round {}: the explicit cache profile is declared",
            round + 1
        );
    }

    // ---- Dimension 2: the tool table. The production surface is
    // demand-driven, so the honest invariants are: (a) the composition
    // baseline is on EVERY round; (b) only the two legitimate mechanisms
    // ever add anything — the NeedEvidence lease surfacing of
    // `context.manage` (item 24) and the scripted `capability.manage load`
    // of the catalog-cold `fs.mkdir`; (c) a tool that appears on two
    // rounds carries the SAME schema both times; (d) fs.mkdir is surfaced
    // exactly on R11 (the round after the load) and nowhere else. (The
    // transport's name codec spells wire tool names with underscores.) ----
    let base_tools = tool_names(&wires[0]);
    assert!(
        base_tools.contains(&"fs_read".to_string())
            && base_tools.contains(&"fs_write".to_string())
            && base_tools.contains(&"capability_manage".to_string())
            && !base_tools.contains(&"fs_mkdir".to_string()),
        "the production builtin surface is on the wire and fs.mkdir starts catalog-cold: {base_tools:?}"
    );
    let allowed_extras = ["context_manage".to_string(), "fs_mkdir".to_string()];
    let mut schema_by_name: std::collections::HashMap<String, Value> =
        std::collections::HashMap::new();
    for (round, wire) in wires.iter().enumerate() {
        let names = tool_names(wire);
        for name in &base_tools {
            assert!(
                names.contains(name),
                "round {round}: baseline tool {name} dropped off the surface"
            );
        }
        for name in &names {
            assert!(
                base_tools.contains(name) || allowed_extras.contains(name),
                "round {round}: unexpected tool {name} joined the surface"
            );
            let schema = wire["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"].as_str() == Some(name.as_str()))
                .expect("the name came from this wire")
                .clone();
            if let Some(previous) = schema_by_name.insert(name.clone(), schema) {
                assert_eq!(
                    previous, schema_by_name[name],
                    "round {round}: tool {name} changed schema between rounds"
                );
            }
        }
        if round + 1 == 11 {
            assert!(
                names.contains(&"fs_mkdir".to_string()),
                "R11: the loaded tool must be offered on the very next request: {names:?}"
            );
        } else {
            assert!(
                !names.contains(&"fs_mkdir".to_string()),
                "round {}: fs.mkdir must be surfaced only on R11: {names:?}",
                round + 1
            );
        }
    }

    // ---- Dimension 3: the stable prefix boundary. B0 (the stable policy
    // end) must be byte-identical on every round; B1 (the declared evidence
    // end) only moves when evidence legitimately moves. ----
    let breakpoints: Vec<Vec<usize>> = wires.iter().map(breakpoint_positions).collect();
    for (round, positions) in breakpoints.iter().enumerate() {
        assert!(
            !positions.is_empty(),
            "round {}: B0 (the stable policy end) is always declared",
            round + 1
        );
        assert_eq!(
            positions[0],
            breakpoints[0][0],
            "round {}: B0 stays pinned at the same wire item",
            round + 1
        );
        let b0 = positions[0];
        assert!(
            wires
                .iter()
                .all(|wire| wire["input"][b0] == wires[0]["input"][b0]),
            "round {}: the B0 stable-policy item must be byte-identical across the whole trajectory",
            round + 1
        );
    }
    let b0_index = breakpoints[0][0];
    let body_of = |round: usize| wires[round]["input"].to_string();

    // R1: the trajectory baseline carries the directive and nothing else.
    assert_sentinels(&body_of(0), &["T1 report the alpha row"], 1);
    assert!(
        breakpoints[0].len() == 1,
        "R1 runs before any evidence exists — an empty selection declares only B0"
    );

    // R1 -> R2: the turn grew by the alpha read exchange; the alpha body
    // (the read RESULT) is really on the wire — the read is proven by its
    // body, not by the fact that a call was emitted.
    assert_sentinels(&body_of(1), &["ALPHA-REV1-SENTINEL"], 2);
    let diff = first_input_diff(&wires[0], &wires[1]).expect("R2 differs from R1");
    assert!(
        diff > b0_index,
        "R1->R2: the first difference must sit after the stable policy (got index {diff})"
    );

    // R2 -> R3: the write exchange joined the turn; the write RESULT names
    // the artifact (the write itself is proven by disk, not by wire text).
    assert_sentinels(&body_of(2), &["file updated: evidence.txt"], 3);

    // R3 -> R4: turn 2 begins; turn 1's observations were ingested at its
    // commit, so the DECLARED EVIDENCE REGION itself now appears: R4 is
    // the first round that declares B1, the item right after B0, and it
    // is the SELECTED WORKING CONTEXT block carrying the turn-1 read
    // observation. This is the "new evidence enters" wire fact.
    let diff = first_input_diff(&wires[2], &wires[3]).expect("R4 differs from R3");
    assert!(
        diff > b0_index,
        "R3->R4: the first difference must sit after the stable policy (got index {diff})"
    );
    assert_eq!(
        breakpoints[3],
        vec![b0_index, b0_index + 1],
        "R4: B1 is declared right after B0 once evidence exists"
    );
    let r4_evidence = item_text(&wires[3]["input"][b0_index + 1]);
    assert!(
        r4_evidence.starts_with("SELECTED WORKING CONTEXT")
            && r4_evidence.contains("ALPHA-REV1-SENTINEL"),
        "R4: the declared evidence item must carry the turn-1 read observation: {r4_evidence}"
    );

    // R4 -> R5: the beta read exchange joined the turn — the NEW evidence
    // body is really present (body integrity for the new evidence).
    assert_sentinels(&body_of(4), &["BETA-NEW-EVIDENCE-SENTINEL"], 5);

    // R5 -> R6: the steering turn. The new directive is on the wire; the
    // stable policy stays put.
    let diff = first_input_diff(&wires[4], &wires[5]).expect("R6 differs from R5");
    assert!(
        diff > b0_index,
        "R5->R6: the first difference must sit after the stable policy (got index {diff})"
    );
    assert_sentinels(&body_of(5), &["T3 archive focus now"], 6);

    // R6 -> R7: turn 4 begins with the rewrite call.
    let diff = first_input_diff(&wires[5], &wires[6]).expect("R7 differs from R6");
    assert!(
        diff > b0_index,
        "R6->R7: the first difference must sit after the stable policy (got index {diff})"
    );

    // R7 -> R8: the rewrite exchange joined the turn.
    assert_sentinels(&body_of(7), &["file updated: notes/alpha.txt"], 8);

    // R8 -> R9: the re-read exchange joined — the NEW version body is on
    // the wire; the superseded rev1 body must not ride THIS turn's fresh
    // read (the read result is rev2).
    assert_sentinels(&body_of(8), &[ALPHA_REV2.trim()], 9);

    // R9 -> R10: turn 5 begins; turn 4's observations were ingested at its
    // commit, so the DECLARED EVIDENCE now reflects the file version
    // change: the rev2 body is what the evidence item carries.
    let diff = first_input_diff(&wires[8], &wires[9]).expect("R10 differs from R9");
    assert!(
        diff > b0_index,
        "R9->R10: the first difference must sit after the stable policy (got index {diff})"
    );
    let r10_evidence = item_text(&wires[9]["input"][b0_index + 1]);
    assert!(
        r10_evidence.contains(ALPHA_REV2.trim()),
        "R10: the declared evidence must carry the rewritten (rev2) file body: {r10_evidence}"
    );

    // R10 -> R11 / R11 -> R12: the tool surface changed between these
    // rounds (asserted above); the messages keep carrying the exchange
    // receipts.
    assert_sentinels(&body_of(10), &["tool loaded: fs.mkdir"], 11);
    assert_sentinels(&body_of(11), &["tool unloaded: fs.mkdir"], 12);

    // R12 -> R13: the checkpoint/restore step. Same key (asserted above);
    // the stable policy AND the tool table survive the teardown unchanged.
    assert_eq!(
        wires[12]["input"][b0_index], wires[11]["input"][b0_index],
        "R13: the B0 policy item survives the checkpoint/restore byte-identically"
    );
    for name in &base_tools {
        assert!(
            tool_names(&wires[12]).contains(name),
            "R13: baseline tool {name} must survive the restore on the surface"
        );
    }
    assert!(
        !tool_names(&wires[12]).contains(&"fs_mkdir".to_string()),
        "R13: the tool catalog is composition-level — fs.mkdir is back to catalog-cold after the recompose"
    );
    assert_eq!(
        breakpoints[12],
        vec![b0_index, b0_index + 1],
        "R13: the declared evidence region itself survives the restore"
    );
    let r13_evidence = item_text(&wires[12]["input"][b0_index + 1]);
    assert!(
        r13_evidence.starts_with("SELECTED WORKING CONTEXT"),
        "R13: the restored evidence item is the declared SELECTED WORKING CONTEXT block: {r13_evidence}"
    );
    let diff = first_input_diff(&wires[11], &wires[12]).expect("R13 differs from R12");
    assert!(
        diff > b0_index,
        "R12->R13: the restore may only rewrite the tail, never the declared policy (got index {diff})"
    );

    // R13 -> R14: the artifact read exchange joined — the DISK-verified
    // artifact bytes now really participate in the request body again,
    // after the restore. This is the anti-fabrication half of the fs.write
    // proof: the write's bytes were asserted on disk after turn 1, and
    // here they come back through a real read into a real request.
    assert_sentinels(&body_of(13), &[EVIDENCE_V1], 14);

    // ---- Dimension 3b (tenth batch, class 1): the ENTIRE stable prefix,
    // not just the breakpoint item. The ninth-batch checks compared
    // `input[B0]` (plus sampled adjacent first-differences); the items
    // before the breakpoint were a blind spot. Three comparisons:
    // (a) every round's whole input[0..=B0] against the trajectory
    //     baseline, item for item;
    // (b) every adjacent pair's whole DECLARED COMMON prefix — within a
    //     turn it must be item-for-item identical (the declared evidence
    //     region may not move while the turn runs);
    // (c) at a turn start the pair may diverge ONLY at the declared
    //     evidence item itself, and the re-rendered item must keep the
    //     declared structure. Every failure names the first divergent
    //     item index. The participating tools/schema blocks are compared
    //     as one canonical whole whenever the same tool set is offered. ----
    for round in 1..wires.len() {
        if let Some(index) = first_prefix_divergence(&wires[0], &wires[round], b0_index) {
            panic!(
                "round {}: the FULL stable prefix input[0..=B0] diverges from the trajectory \
                 baseline at item {index} (the old check compared only input[B0]):\n  baseline: {}\n  round:    {}",
                round + 1,
                item_preview(&wires[0]["input"][index]),
                item_preview(&wires[round]["input"][index]),
            );
        }
    }
    let turn_starts = turn_start_rounds(wires.len());
    for previous in 0..wires.len() - 1 {
        let current = previous + 1;
        let common_end = breakpoints[previous]
            .iter()
            .filter(|index| breakpoints[current].contains(index))
            .copied()
            .max()
            .expect("every round declares at least B0");
        let divergence = first_prefix_divergence(&wires[previous], &wires[current], common_end);
        let current_is_turn_start = turn_starts.contains(&(current + 1));
        let evidence_index = b0_index + 1;
        let allowed = match divergence {
            None => true,
            // Mid-turn rounds: nothing inside the declared common prefix
            // may change at all.
            Some(_) if !current_is_turn_start => false,
            // A turn start may re-render the declared evidence item, and
            // only that item — the declared structure must survive.
            Some(index) if index == evidence_index => {
                let text = item_text(&wires[current]["input"][evidence_index]);
                text.starts_with("SELECTED WORKING CONTEXT")
            }
            Some(_) => false,
        };
        assert!(
            allowed,
            "rounds {}->{}: the declared stable prefix input[0..={}] diverged at item {:?} \
             (first divergent index; {} — baseline item: {}, changed item: {})",
            previous + 1,
            current + 1,
            common_end,
            divergence,
            if current_is_turn_start {
                "at a turn start only the declared evidence item may change"
            } else {
                "mid-turn: nothing in the declared prefix may change"
            },
            divergence
                .map(|index| item_preview(&wires[previous]["input"][index]))
                .unwrap_or_default(),
            divergence
                .map(|index| item_preview(&wires[current]["input"][index]))
                .unwrap_or_default(),
        );
        // The participating tools compare as one canonical block whenever
        // the same tool set is offered on both rounds (a per-name schema
        // drift, a reordered-but-equal table, or a name-set-stable rewrite
        // all fail here with the tool named).
        let previous_tools = canonical_tools(&wires[previous]);
        let current_tools = canonical_tools(&wires[current]);
        if previous_tools.keys().eq(current_tools.keys()) {
            for (name, schema) in &current_tools {
                assert_eq!(
                    previous_tools.get(name),
                    Some(schema),
                    "rounds {}->{}: the participating tools block changed for {name} while the tool set stayed the same",
                    previous + 1,
                    current + 1
                );
            }
        }
    }

    // ---- Dimension 4 (tenth batch, class 3): the FULL cost ledger — one
    // known row per SERVED decision, settled exactly once, never invented,
    // now recording per row the provider cache read/write/miss buckets
    // (with EXPLICIT unknown markers for unreported counters), the real
    // attempt count and the call lane. The scripted usage numbers prove
    // TRANSPORT and SETTLEMENT only — no assertion here says anything
    // about real cache hit rates or price savings, and
    // ENDPOINT_ACCEPTED / SERVER_HIT / NET_TASK_COST stay NOT_RUN. ----
    let rows = ledger.lock().unwrap().clone();
    let expected_rows: Vec<LedgerRow> = served
        .iter()
        .map(|script| {
            let (input_tokens, output_tokens) = script.usage();
            LedgerRow {
                input_tokens,
                output_tokens,
                identity: UsageIdentity::Observed,
                // The scripted SSE server reports input/output only: every
                // cache bucket stays UNKNOWN — an unreported counter is
                // never an invented zero, and these rows must never be
                // read as real cache behavior.
                cached_input_tokens: CostCounter::Unknown,
                cache_write_input_tokens: CostCounter::Unknown,
                cache_miss_input_tokens: CostCounter::Unknown,
                event_cached_input_tokens: 0,
                // Exactly one real transport attempt per round: the capture
                // server saw exactly one body per served round and
                // nothing extra (asserted above).
                attempts: 1,
                retries: 0,
                // Every round of this trajectory is a main-lane round;
                // this composition attaches no compactor, so no
                // maintenance call may ever appear.
                role: ModelCallRole::Main,
                typed_usage_reported: true,
            }
        })
        .collect();
    assert_eq!(
        rows, expected_rows,
        "every served round settles exactly its own reported counters, in order, once — \
         now including the cache buckets, real attempt counts and call lane"
    );
    // The fixed segments really ran as scripted, in their places: the pre
    // segment (turns 1-6 + the walk opener) first, the billed failure last,
    // with only the generated walk segment between.
    assert_eq!(
        &served[..pre_scripts.len()],
        &pre_scripts[..],
        "the fixed pre-segment (turns 1-6 + the walk opener) must be served first, verbatim"
    );
    assert_eq!(
        served.last(),
        post_scripts.first(),
        "the billed terminal failure must be the trajectory's final served decision"
    );
    // The dynamic walk segment: continuation calls up to the completion.
    let walk_segment = &served[pre_scripts.len()..served.len() - 1];
    let completion_ok = matches!(
        walk_segment.last(),
        Some(Script::Text {
            delta: "t6b complete",
            ..
        })
    );
    assert!(
        completion_ok,
        "the generated walk segment must end with the walk completion"
    );
    assert!(
        walk_segment[..walk_segment.len() - 1]
            .iter()
            .all(|script| matches!(
                script,
                Script::Call {
                    name: "fs.read",
                    ..
                }
            )),
        "every generated walk round except the completion must be an fs.read continuation call"
    );
    // Totals: the ledger totals are exactly the sum of the per-row values,
    // and the per-row values are exactly the served wire usage.
    let rows_input_total: u64 = rows.iter().map(|row| row.input_tokens).sum();
    let rows_output_total: u64 = rows.iter().map(|row| row.output_tokens).sum();
    let served_input_total: u64 = served.iter().map(|script| script.usage().0).sum();
    let served_output_total: u64 = served.iter().map(|script| script.usage().1).sum();
    assert_eq!(
        rows_input_total, served_input_total,
        "the ledger input total must equal the sum of the per-row values ({rows_input_total} != {served_input_total})"
    );
    assert_eq!(
        rows_output_total, served_output_total,
        "the ledger output total must equal the sum of the per-row values ({rows_output_total} != {served_output_total})"
    );
    // No cumulative usage snapshot counted twice: every settled row keeps
    // its own unique input-token identity (a re-emitted cumulative
    // snapshot would duplicate one).
    let unique_tokens: std::collections::BTreeSet<u64> =
        rows.iter().map(|row| row.input_tokens).collect();
    assert_eq!(
        unique_tokens.len(),
        rows.len(),
        "one settled row per wire round with a unique token identity: a cumulative usage \
         snapshot must not be counted twice"
    );
    // Unknown accounting: EVERY cache bucket is Unknown on EVERY row — the
    // known-cache totals are empty FACTS, not zeros, and the ledger says
    // so explicitly rather than summing absent counters as zero.
    let fully_unknown_cache_rows = rows
        .iter()
        .filter(|row| {
            matches!(row.cached_input_tokens, CostCounter::Unknown)
                && matches!(row.cache_write_input_tokens, CostCounter::Unknown)
                && matches!(row.cache_miss_input_tokens, CostCounter::Unknown)
        })
        .count();
    assert_eq!(
        fully_unknown_cache_rows,
        rows.len(),
        "the scripted transport reports no cache buckets: every row must carry the explicit \
         unknown marker for cache read/write/miss (never an invented zero)"
    );
    // Call-lane accounting: the main lane produced every row and the
    // maintenance lane produced none.
    assert!(
        rows.iter().all(|row| row.role == ModelCallRole::Main),
        "every ledger row must be a main-lane round; a maintenance call on this composition \
         (no compactor attached) would be an unscripted model call"
    );
    let maintenance_rows = rows
        .iter()
        .filter(|row| row.role == ModelCallRole::Maintenance)
        .count();
    assert_eq!(
        maintenance_rows, 0,
        "no maintenance-lane row may exist in this trajectory"
    );

    // Diagnostics for the receipt: where B1 was declared and where the
    // first difference landed per step (recorded facts, not claims).
    for (round, wire) in wires.iter().enumerate() {
        let b1_snippet = if breakpoints[round].len() > 1 {
            let text = item_text(&wire["input"][b0_index + 1]);
            text.chars().take(90).collect::<String>()
        } else {
            String::new()
        };
        eprintln!(
            "KV_SEQ round={} breakpoints={:?} tools={} input_items={} b1={:?}",
            round + 1,
            breakpoints[round],
            tool_names(wire).join(","),
            wire["input"].as_array().unwrap().len(),
            b1_snippet,
        );
    }

    // ---- Dimension 5 (tenth batch, class 2): REAL delivery evidence.
    // The scripted model walks the pre-seeded big log EXACTLY along the
    // tool's returned continuation clauses (the product rule under test:
    // "use the returned clause verbatim, walk to true EOF"), and every
    // page's FINAL delivered tool-result content must contain its whole
    // claimed range verbatim: no truncation marker, no head+tail clip,
    // every block ID on its page with its true source line number, and a
    // claims union that covers 1..=600 with no gaps and no overlaps. ----

    // (a) The walk shape, read off the wire: the rounds whose wire INPUT
    // first carries each page request (a call decided in round N enters
    // the input of round N+1, together with its result item). Within the
    // turn the earlier pages' call items ride along in the later
    // requests, so each distinct page window is recorded at its FIRST
    // wire appearance — that ordering IS the walk.
    let mut walk_pages: Vec<(usize, u64, u64)> = Vec::new();
    for (index, wire) in wires.iter().enumerate() {
        for (start, end) in requested_big_log_pages(wire) {
            if !walk_pages
                .iter()
                .any(|(_, seen_start, seen_end)| *seen_start == start && *seen_end == end)
            {
                walk_pages.push((index + 1, start, end));
            }
        }
    }
    assert!(
        walk_pages.len() > 1,
        "the big-log walk must span several continuation pages, got {:?}",
        walk_pages
    );
    // (b) The opener is the scripted window; EVERY later page must equal
    // a previously returned continuation clause, verbatim (the tool
    // finishes the requested window first, so requested windows may
    // re-name the window tail — the DELIVERY union below is what must be
    // contiguous).
    assert_eq!(
        (walk_pages[0].1, walk_pages[0].2),
        BIG_LOG_OPENER_WINDOW,
        "the walk must open with the scripted window request"
    );
    for pair in walk_pages.windows(2) {
        let (round, start, end) = pair[1];
        let clause =
            format!("continue with fs.read path={BIG_LOG_PATH} start_line={start} end_line={end}");
        let clause_round = wires
            .iter()
            .position(|wire| {
                delivered_big_log_pages(wire)
                    .iter()
                    .any(|(_, _, text)| text.contains(&clause))
            })
            .unwrap_or_else(|| {
                panic!(
                    "round {round}: the walk page lines {start}-{end} must EQUAL a previously \
                     returned continuation clause ({clause:?}) — the model must follow the \
                     tool's own continuation, not invent windows"
                )
            });
        assert!(
            clause_round + 1 < round,
            "round {round}: the continuation clause for lines {start}-{end} must have been \
             returned STRICTLY BEFORE the request that issued it (clause seen in round {})",
            clause_round + 1
        );
    }
    // (c) The DELIVERED claims (the tool's honest delivered ranges, e.g.
    // the opener window 1-200 delivers claim lines=1-98/600), deduped at
    // first appearance, must chain contiguously from line 1 to true EOF
    // — the union covers 1..=600 with no gaps and no overlaps.
    let mut delivered_claims: Vec<(usize, u64, u64)> = Vec::new();
    for (index, wire) in wires.iter().enumerate() {
        for (start, end, _) in delivered_big_log_pages(wire) {
            if !delivered_claims
                .iter()
                .any(|(_, seen_start, seen_end)| *seen_start == start && *seen_end == end)
            {
                delivered_claims.push((index + 1, start, end));
            }
        }
    }
    assert_eq!(
        delivered_claims.first().map(|&(_, start, _)| start),
        Some(1),
        "the delivered claims must start at the file's first line"
    );
    for pair in delivered_claims.windows(2) {
        assert_eq!(
            pair[1].1,
            pair[0].2 + 1,
            "delivered claims lines {}-{} -> lines {}-{} must chain contiguously (no gaps, no \
             overlaps in what the model actually received)",
            pair[0].1,
            pair[0].2,
            pair[1].1,
            pair[1].2
        );
    }
    assert_eq!(
        delivered_claims.last().expect("at least one claim").2,
        BIG_LOG_LINES as u64,
        "the delivered claims must end exactly at the file's last line — nothing skipped, \
         nothing re-read"
    );
    assert_eq!(
        delivered_claims.len(),
        walk_pages.len(),
        "every requested walk page must correspond to exactly one delivered claim"
    );
    for ((page_round, _, _), &claim) in walk_pages.iter().zip(&delivered_claims) {
        assert_eq!(
            *page_round, claim.0,
            "requested window (round {page_round}) and its delivered claim lines {}-{} must \
             first appear in the SAME request (the call item and its result enter together)",
            claim.1, claim.2
        );
    }

    // (d) The delivery itself, per DELIVERED claim: the page whose claim
    // first appears in round N (1-based) is delivered in the SAME
    // request. Strictly, per page: exactly one delivered body claims it;
    // the body carries the claim; NO truncation marker (a final-budget
    // page reaches the model verbatim — a head+tail clip here is a
    // REGRESSION); and every block ID sits on its page with its true
    // source line number, never leaking into a page that does not claim
    // its line.
    for (page_no, &(request_round, start, end)) in delivered_claims.iter().enumerate() {
        let page_no = page_no + 1;
        let page_deliveries = delivered_big_log_pages(&wires[request_round - 1]);
        let delivered: Vec<&(u64, u64, String)> = page_deliveries
            .iter()
            .filter(|(claimed_start, claimed_end, _)| {
                *claimed_start == start && *claimed_end == end
            })
            .collect();
        assert_eq!(
            delivered.len(),
            1,
            "round {request_round}: exactly one delivered body must claim page {page_no} \
             (lines {start}-{end})",
        );
        let text = &delivered[0].2;
        assert!(
            text.contains(&format!("lines={start}-{end}/{BIG_LOG_LINES}")),
            "round {request_round}: the delivered page {page_no} body must carry its own \
             continuation claim lines={start}-{end}/{}",
            BIG_LOG_LINES
        );
        assert!(
            !text.contains("runtime truncated"),
            "round {request_round}: page {page_no} (lines {start}-{end}) was clipped before \
             reaching the model (runtime truncation marker present) — under final-budget \
             paging this is a DELIVERY REGRESSION, the page must arrive verbatim"
        );
        let mut page_diagnostics = Vec::new();
        for (line, id) in BIG_LOG_MID_MARKER_LINES
            .iter()
            .chain(BIG_LOG_CONTROL_MARKER_LINES.iter())
        {
            let in_claim = (*line as u64) >= start && (*line as u64) <= end;
            // The body renders `{line:>6} | {source line}` — the block ID
            // must appear with its TRUE source line number (G3 identity).
            let rendered = format!("{line:>6} | row-{line:04} {id}");
            if in_claim {
                assert!(
                    text.contains(&rendered),
                    "round {request_round}: block ID {id} (line {line}) must be delivered ON the \
                     page claiming lines {start}-{end}, with its true source line number \
                     (expected rendered row {rendered:?})"
                );
            } else {
                assert!(
                    !text.contains(id),
                    "round {request_round}: block ID {id} (line {line}) must not leak into the \
                     page claiming lines {start}-{end} — every line is delivered exactly once"
                );
            }
            page_diagnostics.push(format!(
                "L{line}={}",
                if in_claim { "on-page" } else { "-" }
            ));
        }
        eprintln!(
            "KV_SEQ_DELIVERY round={request_round} page={page_no} \
             claim=lines={start}-{end}/{} chars={} markers[{}] clipped=false",
            BIG_LOG_LINES,
            text.chars().count(),
            page_diagnostics.join(" "),
        );
    }
}

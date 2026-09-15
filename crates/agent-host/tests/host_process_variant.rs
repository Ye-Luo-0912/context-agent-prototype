//! S4 (R6 supplement): the INDEPENDENT-PROCESS variant of the T7 journey.
//!
//! What this test PROVES — the facts `host_t7_journey.rs` deliberately does
//! not, because there both "sessions" are server threads inside the test
//! process:
//!
//!   * the endpoint is served by REAL `agent-host` OS processes (spawned
//!     from Cargo's own `CARGO_BIN_EXE_agent-host`, configured exactly like
//!     an operator would: CLI flags + provider environment variables);
//!   * the first host process is KILLED. The kill is observed
//!     deterministically (`Child::wait`), and the PID is then asserted to be
//!     gone via the product's own liveness probe
//!     (`agent_process::process_is_running` — the same probe the
//!     single-instance lock uses to decide a stale takeover);
//!   * the second host process is a DIFFERENT OS process (different PID) and
//!     takes over the same workspace: it starts cold (knows no tasks),
//!     re-initializes the stale single-instance lock left by the killed
//!     process, and restores the SAME TaskId from the checkpoint artifact on
//!     disk, with the response naming the killed process's RunId — one
//!     lineage across a real process death;
//!   * instruction delivery is evidenced, not assumed. The model is a local
//!     scripted HTTP provider (an OpenAI Responses-compatible server on the
//!     loopback) whose EVERY answer is decided by inspecting the request it
//!     actually received:
//!       - the mid-task steer carries a one-time unique marker token, and
//!         the provider REFUSES (HTTP 500) any post-steer request that does
//!         not contain that token verbatim — so a runtime that failed to
//!         hand the correction to the model fails the test right there
//!         instead of the script knowing the answer in advance;
//!       - the first request of the restored process must carry the task
//!         goal, the completed part (`stage.md`), and the steer marker (the
//!         remaining obligation) — the marker must survive checkpoint +
//!         process death + restore;
//!       - the summary file's content is DERIVED from the marker found in
//!         the request, so what lands on disk could only come from what the
//!         runtime actually delivered;
//!       - order causality: the provider and the client append to one shared
//!         ordered log; the test asserts the steer was recorded after some
//!         model request, that no request before the steer contains the
//!         marker, and that the first request after the steer does.
//!
//! What this test does NOT prove: real vendor model quality or provider KV
//! caching (that is T8's conditional work), and it does not replace the
//! same-process journey's wider coverage (cancel barrier, failure variants).
//! The scripted provider answers the OpenAI Responses wire shape proven by
//! `agent-compose`'s cache_wire_flow capture server; no vendor is called.
//!
//! The task is left `Active` at the end: with no in-process RuntimeHandle
//! there is no operator surface here, and under `OperatorClosureOnly` the
//! continuation's ordinary final must not be a durable completion — the
//! process-2 assertions check exactly that.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_platform_protocol::{
    ActiveFeatures, ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse,
    Causality, EnvelopeKind, MessageId, PlatformEnvelope, PlatformResponse, ProtocolIdentity,
    ProtocolVersion, RequestId, Route, TaskSnapshotStatus, WorkCheckpointRequest,
    WorkCheckpointResponse, WorkContinueDisposition, WorkContinueReason, WorkContinueRequest,
    WorkContinueResponse, WorkRestoreRequest, WorkRestoreResponse, WorkSnapshotRequest,
    WorkSnapshotResponse, WorkSteerDisposition, WorkSteerRequest, WorkSteerResponse,
    WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse, WorkTaskDetailRequest,
    WorkTaskDetailResponse,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// The scripted journey's fixed facts.
// ---------------------------------------------------------------------------

/// The submitted goal. Plain ASCII only: it must substring-match the raw
/// JSON wire body verbatim (no characters that JSON escaping would change).
const GOAL: &str = "procvar goal: create stage.md in the workspace, then create \
                    summary.md quoting the stage line; a later correction fixes \
                    the exact first line of summary.md";

/// The steer's prefix; the one-time token is appended at test runtime.
const STEER_PREFIX: &str = "procvar correction";

/// What the model writes in its first (and only) process-1 effect.
const STAGE_CONTENT: &str = "stage line: the brief every later file quotes";

/// The held turn's final: ends the turn so the queued correction drains.
const FINAL_STAGE_HELD: &str =
    "procvar: stage.md is written; ending the turn so the correction can be applied.";

/// The restored process's closing final.
const FINAL_DONE: &str =
    "procvar: summary.md carries the corrected first line; the goal work is complete.";

// ---------------------------------------------------------------------------
// The script: a PURE function from (round, request body) -> decision. The
// provider never answers from a preset schedule alone; every step validates
// what the request actually carried, and refusal is an explicit outcome so
// the test's failure point is "the runtime never handed X to the model".
// ---------------------------------------------------------------------------

/// What the provider does with one received model request.
#[derive(Debug)]
enum Decision {
    /// Answer immediately with this SSE body.
    Answer(String),
    /// Keep the request in flight (the turn stays RUNNING) until the test
    /// releases the hold, then answer with this SSE body.
    Hold(String),
    /// Refuse with HTTP 500. The wrapped transport will retry and then fail
    /// the turn: a missing fact stops the journey instead of being papered
    /// over by a scripted answer.
    Refuse(String),
}

/// One SSE answer carrying a single `fs.write` function call (the raw JSON
/// TEXT arguments shape proven by cache_wire_flow).
fn sse_function_call(call_id: &str, path: &str, content: &str) -> String {
    let arguments = json!({"path": path, "content": content}).to_string();
    let call = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": "fs.write",
            "arguments": arguments,
        }
    });
    let completed = json!({
        "type": "response.completed",
        "response": {"usage": {"input_tokens": 64, "output_tokens": 16}}
    });
    format!(
        "event: response.output_item.done\r\ndata: {call}\r\n\r\n\
         event: response.completed\r\ndata: {completed}\r\n\r\n"
    )
}

/// One SSE answer carrying a plain text final.
fn sse_final_text(text: &str) -> String {
    let delta = json!({
        "type": "response.output_text.delta",
        "output_index": 0,
        "delta": text,
    });
    let completed = json!({
        "type": "response.completed",
        "response": {"usage": {"input_tokens": 64, "output_tokens": 8}}
    });
    format!(
        "event: response.output_text.delta\r\ndata: {delta}\r\n\r\n\
         event: response.completed\r\ndata: {completed}\r\n\r\n"
    )
}

/// The marker occurrence actually found in the request — the provider
/// derives its answer from what the runtime delivered, never from the
/// test's own copy of the token.
fn delivered_marker<'a>(body: &'a str, token: &'a str) -> &'a str {
    body.find(token)
        .map(|pos| &body[pos..pos + token.len()])
        .unwrap_or(token)
}

/// The scripted contract, round by round. `round` counts model requests
/// across BOTH host processes (the provider outlives them).
fn script_step(round: usize, body: &str, goal: &str, token: &str) -> Decision {
    let has_goal = body.contains(goal);
    let has_token = body.contains(token);
    match round {
        // Process 1, turn 1: the submitted goal must reach the model.
        0 if has_goal => {
            Decision::Answer(sse_function_call("pv-write-1", "stage.md", STAGE_CONTENT))
        }
        0 => Decision::Refuse("the first model request never carried the submitted goal".into()),
        // Process 1, turn 1, round 2: hold the turn open so the mid-turn
        // steer is deterministic, then end it so the correction drains.
        1 if has_goal => Decision::Hold(sse_final_text(FINAL_STAGE_HELD)),
        1 => Decision::Refuse("the held request lost the submitted goal".into()),
        // Process 1, turn 2 (the drained correction): the marker must be in
        // the request — the runtime handed the steer to the model.
        2 => {
            if !has_goal {
                return Decision::Refuse(
                    "the post-correction request lost the submitted goal".into(),
                );
            }
            if !has_token {
                return Decision::Refuse(
                    "the post-correction request never carried the steer marker: \
                     the runtime did not hand the correction to the model"
                        .into(),
                );
            }
            let delivered = delivered_marker(body, token);
            Decision::Answer(sse_final_text(&format!(
                "acknowledged: {delivered}; summary.md will be written when the task continues"
            )))
        }
        // Process 2, first post-restore request: the remaining-task facts —
        // the goal, the completed part, and the surviving obligation.
        3 => {
            if !has_goal {
                return Decision::Refuse("the restored request lost the submitted goal".into());
            }
            if !body.contains("stage.md") {
                return Decision::Refuse(
                    "the restored request never referenced the completed stage.md".into(),
                );
            }
            if !has_token {
                return Decision::Refuse(
                    "the restored request never carried the steer marker: the remaining \
                     obligation did not survive the process boundary"
                        .into(),
                );
            }
            let delivered = delivered_marker(body, token);
            Decision::Answer(sse_function_call(
                "pv-write-2",
                "summary.md",
                &format!("correction: {delivered}"),
            ))
        }
        // Process 2, after the summary write: finish the continuation.
        4 if has_token => Decision::Answer(sse_final_text(FINAL_DONE)),
        4 => Decision::Refuse("the closing request lost the steer marker".into()),
        other => Decision::Refuse(format!("unexpected extra model round {other}")),
    }
}

// ---------------------------------------------------------------------------
// The provider's shared ordered log — the causality record.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A model request arrived at the provider.
    ModelRequest,
    /// The test's steer was acknowledged on the wire.
    Steer,
}

#[derive(Debug)]
struct Entry {
    kind: EntryKind,
    has_token: bool,
    detail: String,
}

#[derive(Debug, Default)]
struct ProviderLog {
    entries: Vec<Entry>,
    /// Total model requests seen (the round counter).
    model_rounds: usize,
    /// Every refusal, with its reason — must be empty in the green path.
    refusals: Vec<String>,
}

impl ProviderLog {
    fn record(&mut self, kind: EntryKind, has_token: bool, detail: String) {
        self.entries.push(Entry {
            kind,
            has_token,
            detail,
        });
    }
}

// ---------------------------------------------------------------------------
// The scripted HTTP provider (OpenAI Responses wire shape on the loopback).
// ---------------------------------------------------------------------------

/// Upper bound for one held model request: comfortably above the steer +
/// snapshot round trip, far below the provider transport's own 120s timeout.
const HOLD_BUDGET: Duration = Duration::from_secs(90);

const REFUSED_RESPONSE: &str =
    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

fn ok_sse(sse: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}")
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

/// Waits for the test's release of a held request. `false` on timeout or a
/// dropped sender — both become refusals, never a silent success.
async fn wait_released(hold: &watch::Sender<bool>) -> bool {
    let mut rx = hold.subscribe();
    let wait = async {
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return false;
            }
        }
        true
    };
    tokio::time::timeout(HOLD_BUDGET, wait)
        .await
        .unwrap_or(false)
}

async fn handle_connection(
    socket: &mut tokio::net::TcpStream,
    log: Arc<Mutex<ProviderLog>>,
    hold: Arc<watch::Sender<bool>>,
    goal: &str,
    token: &str,
) {
    // Read exactly one HTTP request: headers, then content-length bytes.
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buffer.extend_from_slice(&chunk[..n]);
                if let Some(header_end) = find_subsequence(&buffer, b"\r\n\r\n") {
                    let wanted = content_length(&buffer[..header_end]);
                    if buffer[header_end + 4..].len() >= wanted {
                        break;
                    }
                }
            }
        }
    }
    let Some(body_start) = find_subsequence(&buffer, b"\r\n\r\n") else {
        return;
    };
    let body = String::from_utf8_lossy(&buffer[body_start + 4..]).into_owned();

    let (round, decision) = {
        let mut log = log.lock().expect("provider log mutex");
        let round = log.model_rounds;
        log.model_rounds += 1;
        log.record(
            EntryKind::ModelRequest,
            body.contains(token),
            format!("model round {round} arrived"),
        );
        (round, script_step(round, &body, goal, token))
    };

    let http = match decision {
        Decision::Refuse(reason) => {
            log.lock()
                .expect("provider log mutex")
                .refusals
                .push(format!("model round {round}: {reason}"));
            REFUSED_RESPONSE.to_string()
        }
        Decision::Hold(sse) => {
            if wait_released(&hold).await {
                ok_sse(&sse)
            } else {
                log.lock()
                    .expect("provider log mutex")
                    .refusals
                    .push(format!(
                        "model round {round}: the mid-turn hold was never released"
                    ));
                REFUSED_RESPONSE.to_string()
            }
        }
        Decision::Answer(sse) => ok_sse(&sse),
    };
    let _ = socket.write_all(http.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// Binds the scripted provider on an ephemeral loopback port and serves it
/// until the test runtime ends. Returns the port.
async fn spawn_script_provider(
    log: Arc<Mutex<ProviderLog>>,
    hold: Arc<watch::Sender<bool>>,
    goal: Arc<str>,
    token: Arc<str>,
) -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let log = Arc::clone(&log);
            let hold = Arc::clone(&hold);
            let goal = Arc::clone(&goal);
            let token = Arc::clone(&token);
            tokio::spawn(async move {
                handle_connection(&mut socket, log, hold, &goal, &token).await;
            });
        }
    });
    Ok(port)
}

// ---------------------------------------------------------------------------
// Real host processes: spawn, readiness, kill, liveness.
// ---------------------------------------------------------------------------

/// The connect budget for a cold host process start: workspace open, engine
/// build and the first bind on a loaded runner.
const CONNECT_BUDGET: Duration = Duration::from_secs(90);

struct HostProcess {
    child: tokio::process::Child,
    pid: u32,
    stderr_text: Arc<Mutex<String>>,
}

/// One unique endpoint for this test run (the SERIAL/nanos naming pattern
/// from the t7 journey: parallel tests must never share an endpoint name).
fn unique_suffix(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SERIAL: std::sync::atomic::AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    format!("{tag}-{nanos:x}-{serial}-{}", std::process::id())
}

#[cfg(windows)]
fn procvar_endpoint(tag: &str) -> agent_host::LocalEndpoint {
    agent_host::LocalEndpoint::NamedPipe(format!("focus-agent-procvar-{}", unique_suffix(tag)))
}

#[cfg(unix)]
fn procvar_endpoint(tag: &str) -> agent_host::LocalEndpoint {
    agent_host::LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-procvar-{}.sock", unique_suffix(tag))),
    )
}

/// Spawns the REAL host binary against the scripted provider, exactly like
/// an operator would: CLI flags for workspace + endpoint, environment
/// variables for the model. Stderr is captured for failure diagnostics.
async fn spawn_host_process(
    workdir: &std::path::Path,
    endpoint: &agent_host::LocalEndpoint,
    provider_port: u16,
) -> anyhow::Result<HostProcess> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-host"));
    command.arg("--workdir").arg(workdir);
    match endpoint {
        agent_host::LocalEndpoint::NamedPipe(name) => {
            command.arg("--pipe").arg(name);
        }
        agent_host::LocalEndpoint::UnixSocket(path) => {
            command.arg("--socket").arg(path);
        }
    }
    command
        // The demo mock must never shadow the scripted provider.
        .env_remove("AGENT_DEMO")
        .env("OPENAI_API_KEY", "procvar-scripted-key")
        .env(
            "OPENAI_BASE_URL",
            format!("http://127.0.0.1:{provider_port}/v1"),
        )
        .env("OPENAI_MODEL", "procvar-scripted-model")
        .env("OPENAI_API_PROTOCOL", "responses")
        // A scripted provider must never receive an engine maintenance call:
        // 0 disables compaction sends entirely (the documented knob).
        .env("MAINTENANCE_MAX_CALLS_PER_MAINTAIN", "0")
        .env_remove("MAINTENANCE_MAX_TOKENS_PER_MAINTAIN")
        .env_remove("MAINTENANCE_COMPACT_FAILURE_BACKOFF")
        .env_remove("MAINTENANCE_TIMEOUT_SECS")
        // The loopback provider must not be routed through a system proxy
        // (documented repo-wide Windows issue; same rule as cache_wire_flow).
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // A panicking test must not leak a serving host process.
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("the host process spawned without a pid"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("the host process stderr was not captured"))?;
    let stderr_text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&stderr_text);
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;

        let mut stderr = stderr;
        let mut buffer = Vec::new();
        // Ends at process exit (EOF); the bound only guards the reader task.
        if tokio::time::timeout(Duration::from_secs(900), stderr.read_to_end(&mut buffer))
            .await
            .is_ok()
        {
            *sink.lock().expect("stderr sink mutex") =
                String::from_utf8_lossy(&buffer).into_owned();
        }
    });
    Ok(HostProcess {
        child,
        pid,
        stderr_text,
    })
}

/// Polls until the host's endpoint accepts a connection; an early process
/// exit fails immediately with its captured stderr.
async fn wait_host_ready(
    process: &mut HostProcess,
    endpoint: &agent_host::LocalEndpoint,
    what: &str,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = process.child.try_wait()? {
            let pid = process.pid;
            let stderr = process
                .stderr_text
                .lock()
                .expect("stderr sink mutex")
                .clone();
            anyhow::bail!("{what} (pid {pid}) exited early with {status}; stderr:\n{stderr}");
        }
        if try_connect_once(endpoint) {
            return Ok(());
        }
        anyhow::ensure!(
            started.elapsed() < CONNECT_BUDGET,
            "{what} never became connectable within {CONNECT_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Kills the host process, observes the exit deterministically, and then
/// asserts the PID no longer exists via the product's own liveness probe —
/// the same probe the single-instance lock uses for stale takeover.
async fn kill_and_confirm_exit(mut process: HostProcess, what: &str) -> anyhow::Result<()> {
    let pid = process.pid;
    process.child.kill().await.ok();
    let status = process.child.wait().await?;
    // Release our own handle before probing liveness.
    drop(process.child);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while agent_process::process_is_running(pid) {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "{what} (pid {pid}) still exists after the kill was observed ({status})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    eprintln!("procvar: {what} (pid {pid}) exited ({status}); pid confirmed gone");
    Ok(())
}

// ---------------------------------------------------------------------------
// Wire client (the long-flow pattern from host_e2e / host_t7_journey).
// ---------------------------------------------------------------------------

fn client_protocol() -> ProtocolIdentity {
    ProtocolIdentity {
        name: "focus-agent.platform".into(),
        version: ProtocolVersion { major: 1, minor: 0 },
        active_features: ActiveFeatures::default(),
        schema_digest: agent_host::session_schema_digest(),
    }
}

fn request<P>(namespace: &str, operation: &str, payload: P) -> PlatformEnvelope<P> {
    let message_id = MessageId::new();
    PlatformEnvelope {
        protocol: client_protocol(),
        message_id,
        request_id: Some(RequestId::new()),
        kind: EnvelopeKind::Request,
        route: Route {
            namespace: namespace.into(),
            operation: operation.into(),
        },
        work: None,
        causality: Causality::root(message_id),
        payload,
    }
}

fn expect_value<T>(response: PlatformResponse<T>) -> T {
    match response {
        PlatformResponse::Success { value } => value,
        PlatformResponse::Error { error } => panic!("expected success, got error {error:?}"),
    }
}

fn exchange<S: Read + Write, P: serde::Serialize, R: serde::de::DeserializeOwned>(
    stream: &mut S,
    request: &PlatformEnvelope<P>,
) -> anyhow::Result<PlatformResponse<R>> {
    agent_host::write_frame(stream, &serde_json::to_vec(request)?)?;
    let frame = agent_host::read_frame(stream)?
        .ok_or_else(|| anyhow::anyhow!("connection closed before response"))?;
    let envelope = serde_json::from_slice::<PlatformEnvelope<PlatformResponse<R>>>(&frame)?;
    assert_eq!(
        &envelope.request_id, &request.request_id,
        "correlated response"
    );
    assert_eq!(envelope.kind, EnvelopeKind::Response);
    Ok(envelope.payload)
}

fn snapshot_request() -> PlatformEnvelope<WorkSnapshotRequest> {
    request("work", "snapshot", WorkSnapshotRequest {})
}

fn readiness_of(snapshot: &WorkSnapshotResponse) -> WorkContinueReason {
    snapshot
        .continue_readiness
        .as_ref()
        .map(|readiness| readiness.reason)
        .expect("the snapshot reports why continuation is or is not available")
}

#[cfg(windows)]
async fn connect(endpoint: &agent_host::LocalEndpoint) -> std::fs::File {
    let agent_host::LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    let path = format!(r"\\.\pipe\{name}");
    let started = std::time::Instant::now();
    while started.elapsed() < CONNECT_BUDGET {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            return file;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("named pipe {path} never became connectable");
}

#[cfg(unix)]
async fn connect(endpoint: &agent_host::LocalEndpoint) -> std::os::unix::net::UnixStream {
    let agent_host::LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    let started = std::time::Instant::now();
    while started.elapsed() < CONNECT_BUDGET {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(path) {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("uds socket {} never became connectable", path.display());
}

#[cfg(windows)]
fn try_connect_once(endpoint: &agent_host::LocalEndpoint) -> bool {
    let agent_host::LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{name}"))
        .is_ok()
}

#[cfg(unix)]
fn try_connect_once(endpoint: &agent_host::LocalEndpoint) -> bool {
    let agent_host::LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Answers whatever approval is pending over the wire.
async fn answer_pending_approval<S: Read + Write>(
    stream: &mut S,
) -> anyhow::Result<Option<WorkSnapshotResponse>> {
    let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        stream,
        &snapshot_request(),
    )?);
    if let Some(pending) = snapshot.pending_approvals.first() {
        let answered = expect_value(exchange::<_, _, ApprovalRespondResponse>(
            stream,
            &request(
                "approval",
                "respond",
                ApprovalRespondRequest {
                    request_id: pending.request_id.clone(),
                    decision: agent_contracts::ApprovalDecision::Allow,
                },
            ),
        )?);
        assert_eq!(answered.outcome, ApprovalRespondOutcome::Delivered);
        return Ok(None);
    }
    Ok(Some(snapshot))
}

/// Drives the runtime to idle, answering wire approvals on the way — the
/// GUI's real loop, against a host that is a different OS process.
async fn drive_turn_to_idle<S: Read + Write>(
    stream: &mut S,
    what: &str,
) -> anyhow::Result<WorkSnapshotResponse> {
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(snapshot) = answer_pending_approval(stream).await?
            && readiness_of(&snapshot) != WorkContinueReason::TurnRunning
        {
            return Ok(snapshot);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the runtime never left the running state while waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until a committed fs.write artifact carries the expected bytes.
async fn wait_file_content(
    path: &std::path::Path,
    expected: &str,
    what: &str,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if std::fs::read_to_string(path)
            .map(|content| content == expected)
            .unwrap_or(false)
        {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the committed effect never landed for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits until the provider has seen at least `count` model requests —
/// closes the between-turns gap a snapshot can land in.
async fn wait_model_rounds(
    log: &Mutex<ProviderLog>,
    count: usize,
    what: &str,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if log.lock().expect("provider log mutex").model_rounds >= count {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the provider never received model round {count} while waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Committed change rows in the workspace journal naming `needle` — the
/// durable effect record, independent of file content.
fn change_rows(root: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(root.join(".focus-agent").join("changes.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

// ---------------------------------------------------------------------------
// The journey.
// ---------------------------------------------------------------------------

async fn procvar_journey() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();

    // ---- The scripted provider (outlives both host processes) ----
    let token = format!("pvtk-{}", unique_suffix("tok"));
    let steer_instruction = format!(
        "{STEER_PREFIX} {token}: the first line of summary.md must be exactly correction: {token}"
    );
    let log = Arc::new(Mutex::new(ProviderLog::default()));
    let (hold_tx, _hold_rx) = watch::channel(false);
    let port = spawn_script_provider(
        Arc::clone(&log),
        Arc::new(hold_tx.clone()),
        Arc::from(GOAL),
        Arc::from(token.as_str()),
    )
    .await?;
    eprintln!("procvar: scripted provider on 127.0.0.1:{port}; steer token {token}");

    // ==================== HOST PROCESS 1 ====================
    let endpoint1 = procvar_endpoint("1");
    let mut host1 = spawn_host_process(&root, &endpoint1, port).await?;
    let pid1 = host1.pid;
    wait_host_ready(&mut host1, &endpoint1, "host process 1").await?;
    let mut stream = connect(&endpoint1).await;
    eprintln!("procvar: host process 1 serving, pid {pid1}");

    // 1. SUBMIT: the goal creates and focuses ONE task.
    let submitted = expect_value(exchange::<_, _, WorkSubmitResponse>(
        &mut stream,
        &request(
            "work",
            "submit",
            WorkSubmitRequest {
                goal: GOAL.into(),
                client_request_id: "procvar-1".into(),
            },
        ),
    )?);
    assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
    let task_id = submitted.task_id;
    eprintln!("procvar[pid {pid1}]: submitted -> task {task_id}");

    // 2. The first model round writes stage.md; the write parks in the
    //    interactive approval gate until THIS client answers on the wire.
    //    The committed artifact is the steer's causal anchor.
    let stage_path = root.join("stage.md");
    let stage_deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        answer_pending_approval(&mut stream).await?;
        if stage_committed(&stage_path) {
            break;
        }
        anyhow::ensure!(
            std::time::Instant::now() < stage_deadline,
            "stage.md never committed through the wire approval"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    wait_file_content(&stage_path, STAGE_CONTENT, "stage.md before the steer").await?;
    eprintln!("procvar[pid {pid1}]: stage.md committed through the wire approval");

    // 3. MID-TURN CORRECTION with a one-time marker. The provider holds the
    //    running turn's second request open, so the turn cannot end by
    //    itself: the steer rides the running turn's correction slot.
    let steered = expect_value(exchange::<_, _, WorkSteerResponse>(
        &mut stream,
        &request(
            "work",
            "steer",
            WorkSteerRequest {
                instruction: steer_instruction.clone(),
                expected_task_id: Some(task_id),
            },
        ),
    )?);
    assert!(
        matches!(
            steered.disposition,
            WorkSteerDisposition::Queued | WorkSteerDisposition::Applied
        ),
        "the correction must be admitted into the running turn, got {:?}",
        steered.disposition
    );
    assert_eq!(
        steered.task_id,
        Some(task_id),
        "the correction names its task"
    );
    log.lock().expect("provider log mutex").record(
        EntryKind::Steer,
        true,
        "work/steer acknowledged".into(),
    );
    eprintln!("procvar[pid {pid1}]: steer acknowledged; releasing the held request");

    // 4. Release the held request: the held turn ends with its final and the
    //    queued correction drains into turn 2, whose request the provider
    //    checks for the marker.
    hold_tx.send(true).expect("hold receiver alive");
    drive_turn_to_idle(&mut stream, "the corrected turn").await?;
    wait_model_rounds(&log, 3, "the post-correction request").await?;
    let after_steer = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert_eq!(
        after_steer.tasks.len(),
        1,
        "steering must never create a second task"
    );
    eprintln!(
        "procvar[pid {pid1}]: correction turn completed; marker delivery checked by the provider"
    );

    // 5. CHECKPOINT: a formal cross-plane artifact in the run's own store.
    let run_id_1 = after_steer.run_id;
    let captured = expect_value(exchange::<_, _, WorkCheckpointResponse>(
        &mut stream,
        &request("work", "checkpoint", WorkCheckpointRequest {}),
    )?);
    assert_eq!(captured.run_id, run_id_1);
    assert!(captured.payload_bytes > 0);
    assert!(captured.tasks >= 1, "the artifact carries the task rows");
    let artifact = captured.artifact.clone();
    let artifact_on_disk = root
        .join(".focus-agent")
        .join("checkpoints")
        .join(&artifact);
    assert!(
        artifact_on_disk.exists(),
        "the checkpoint artifact exists on disk: {}",
        artifact_on_disk.display()
    );
    eprintln!("procvar[pid {pid1}]: checkpoint {artifact} saved");

    // 6. PROCESS DEATH: kill host 1, observe the exit deterministically,
    //    and assert the PID is gone via the product's own liveness probe.
    drop(stream);
    kill_and_confirm_exit(host1, "host process 1").await?;

    // ==================== HOST PROCESS 2 ====================
    // 7. A DIFFERENT OS process takes over the same workspace: it must
    //    re-initialize the stale single-instance lock the killed process
    //    left behind and start cold.
    let endpoint2 = procvar_endpoint("2");
    let mut host2 = spawn_host_process(&root, &endpoint2, port).await?;
    let pid2 = host2.pid;
    assert_ne!(
        pid2, pid1,
        "the second host is a genuinely different OS process"
    );
    wait_host_ready(&mut host2, &endpoint2, "host process 2").await?;
    let mut stream = connect(&endpoint2).await;
    eprintln!("procvar: host process 2 serving, pid {pid2} (after pid {pid1} died)");

    let cold = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert!(
        cold.focus.is_none() && cold.tasks.is_empty(),
        "before the restore the NEW PROCESS knows no tasks"
    );
    assert_ne!(cold.run_id, run_id_1, "a new process is a new run");

    // 8. RESTORE from disk: the response names the KILLED process's run —
    //    one lineage across a real process death.
    let restored = expect_value(exchange::<_, _, WorkRestoreResponse>(
        &mut stream,
        &request(
            "work",
            "restore",
            WorkRestoreRequest {
                artifact: Some(artifact.clone()),
            },
        ),
    )?);
    assert_eq!(restored.artifact, artifact);
    assert_eq!(
        restored.restored_run_id, run_id_1,
        "the restored checkpoint was captured under process 1's run"
    );
    assert!(
        restored.evidence_degraded.is_empty(),
        "no evidence degradation on this restore: {:?}",
        restored.evidence_degraded
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let verified = loop {
        let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
            &mut stream,
            &snapshot_request(),
        )?);
        if snapshot.focus.as_ref().map(|focus| focus.task_id) == Some(task_id) {
            break snapshot;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the SAME TaskId never came back focused in the new process"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        readiness_of(&verified),
        WorkContinueReason::Ready,
        "the restored task is continuable in the new process"
    );
    assert_eq!(
        std::fs::read_to_string(&stage_path)?,
        STAGE_CONTENT,
        "the external change survived real process death"
    );

    // 9. CONTINUE the same task: the remaining work (summary.md with the
    //    corrected first line) runs in the NEW process. The provider checks
    //    the first post-restore request for the goal, the completed part,
    //    and the surviving marker before it answers.
    let resumed = expect_value(exchange::<_, _, WorkContinueResponse>(
        &mut stream,
        &request(
            "work",
            "continue",
            WorkContinueRequest {
                expected_task_id: Some(task_id),
            },
        ),
    )?);
    assert_eq!(resumed.disposition, WorkContinueDisposition::Continued);
    assert_eq!(resumed.task_id, Some(task_id));
    drive_turn_to_idle(&mut stream, "the post-restore continuation").await?;
    let summary = std::fs::read_to_string(root.join("summary.md"))?;
    assert_eq!(
        summary,
        format!("correction: {token}"),
        "summary.md's first line must be derived from the marker the runtime \
         actually delivered to the model"
    );
    assert_eq!(
        change_rows(&root, "stage.md"),
        1,
        "the new process must not replay process 1's committed effect"
    );
    eprintln!("procvar[pid {pid2}]: remaining work completed -> summary = {summary:?}");

    // 10. DELIVERY shape under OperatorClosureOnly: the continuation's
    //     ordinary final is not a durable completion — the restored task is
    //     still Active with its anchor (goal verbatim) intact.
    let detail = expect_value(exchange::<_, _, WorkTaskDetailResponse>(
        &mut stream,
        &request("work", "task_detail", WorkTaskDetailRequest { task_id }),
    )?);
    assert_eq!(detail.task_id, task_id);
    assert_eq!(
        detail.goal, GOAL,
        "the anchor carries the submitted goal verbatim"
    );
    assert_eq!(
        detail.status,
        TaskSnapshotStatus::Active,
        "OperatorClosureOnly: an ordinary final is not a durable completion"
    );

    drop(stream);

    // 11. THE EVIDENCE LEDGER: no refusals, the exact scripted rounds, and
    //     the steer's order causality.
    assert_evidence_ledger(&log);

    // Leave no serving process behind.
    kill_and_confirm_exit(host2, "host process 2").await?;
    eprintln!("procvar: done");
    Ok(())
}

/// The journey's causal-contract checks, read off the provider's ordered
/// log. Sync on purpose: no lock is held across an await point.
fn assert_evidence_ledger(log: &Mutex<ProviderLog>) {
    let log_guard = log.lock().expect("provider log mutex");
    assert!(
        log_guard.refusals.is_empty(),
        "the scripted provider refused to advance: {:?}",
        log_guard.refusals
    );
    assert_eq!(
        log_guard.model_rounds, 5,
        "the journey is exactly five scripted model rounds, got {}",
        log_guard.model_rounds
    );
    let entries = &log_guard.entries;
    let steer_pos = entries
        .iter()
        .position(|entry| entry.kind == EntryKind::Steer)
        .expect("the steer was recorded");
    assert!(
        entries[..steer_pos]
            .iter()
            .any(|entry| entry.kind == EntryKind::ModelRequest),
        "the steer must come after process 1's model requests"
    );
    assert!(
        entries[..steer_pos].iter().all(|entry| !entry.has_token),
        "no request before the steer may contain the marker"
    );
    let first_after = entries[steer_pos + 1..]
        .iter()
        .find(|entry| entry.kind == EntryKind::ModelRequest)
        .expect("a model request followed the steer");
    assert!(
        first_after.has_token,
        "the first request after the steer must carry the marker: {:?}",
        first_after.detail
    );
}

fn stage_committed(stage_path: &std::path::Path) -> bool {
    std::fs::read_to_string(stage_path)
        .map(|content| content == STAGE_CONTENT)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The script contract's rejection paths, verified directly: a missing fact
// is an explicit refusal (HTTP 500 -> the turn fails), so the journey above
// cannot pass on a preset answer when the runtime failed to deliver.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod script_contract {
    use super::*;

    #[test]
    fn first_request_without_the_goal_is_refused() {
        let decision = script_step(0, "a body without the task", GOAL, "pvtk-x");
        assert!(
            matches!(&decision, Decision::Refuse(reason) if reason.contains("goal")),
            "expected a goal refusal, got {decision:?}"
        );
    }

    #[test]
    fn post_correction_request_without_the_marker_is_refused() {
        // The exact failure R6 cares about: the runtime never handed the
        // correction to the model, so the request carries only the goal.
        let body = format!("{GOAL} ... tool result: wrote stage.md ...");
        let decision = script_step(2, &body, GOAL, "pvtk-missing");
        assert!(
            matches!(&decision, Decision::Refuse(reason) if reason.contains("marker")),
            "expected a marker refusal, got {decision:?}"
        );
    }

    #[test]
    fn restored_request_without_the_marker_is_refused() {
        let body = format!("{GOAL} ... restored context mentions stage.md ...");
        let decision = script_step(3, &body, GOAL, "pvtk-missing");
        assert!(
            matches!(&decision, Decision::Refuse(reason) if reason.contains("marker")),
            "expected a marker refusal, got {decision:?}"
        );
    }

    #[test]
    fn valid_restored_request_writes_summary_from_the_delivered_marker() {
        let token = "pvtk-derived-123";
        let body = format!("history ... {GOAL} ... stage.md ... {token} ...");
        let Decision::Answer(sse) = script_step(3, &body, GOAL, token) else {
            panic!("a valid restored request must be answered");
        };
        assert!(sse.contains("fs.write"), "{sse}");
        assert!(sse.contains("summary.md"), "{sse}");
        assert!(
            sse.contains(&format!("correction: {token}")),
            "the write content must be derived from the marker found IN the request: {sse}"
        );
    }

    #[test]
    fn the_held_round_is_a_hold_not_a_refusal() {
        let decision = script_step(1, GOAL, GOAL, "pvtk-x");
        assert!(matches!(decision, Decision::Hold(_)), "{decision:?}");
    }
}

// multi_thread on purpose: the client side uses blocking std IO over the
// local transport; on the default current_thread test runtime that would
// freeze the runtime and deadlock the server-side block_on calls.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_procvar_two_real_host_processes_restore_one_task() {
    procvar_journey().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_procvar_two_real_host_processes_restore_one_task() {
    procvar_journey().await.unwrap();
}

#[cfg(not(any(windows, unix)))]
compile_error!("the host process variant requires a local transport");

//! MCP (Model Context Protocol) support: a `Capability` whose tools are
//! served by an MCP server over stdio (JSON-RPC 2.0, one JSON document per
//! line). The adapter sits behind the same capability/effect/output
//! boundary as every other capability: discovered tool schemas enter the
//! existing catalog (loaded on demand, never injected wholesale into a
//! request), invocations come back through the bounded `ToolOutput`
//! envelope, and the server child runs with a scrubbed environment and a
//! private cwd. Nothing here widens permissions: the adapter's `risk` and
//! the manifest's `permissions` are what the approval gate and the effect
//! fence enforce, exactly as for any capability.

use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Capability, CapabilityInvocationContext,
    CapabilityKind, CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, OperationId, ToolCall, ToolOutput, ToolRisk, ToolSpec,
    validate_capability_id,
};
use agent_platform_protocol::{JsonDecodeBudget, decode_value};
use agent_process::{DEFAULT_CANCEL_ACK_TIMEOUT, HostLifecycle, ProcessSupervisor, RestartCircuit};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

/// MCP protocol version this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default per-request timeout for MCP calls.
pub const DEFAULT_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound on one stdio frame (a JSON-RPC document).
pub const DEFAULT_MCP_MAX_FRAME_BYTES: u64 = 4 * 1024 * 1024;
/// Bounds on one tool's text content (chars) while aggregating; the
/// kernel output broker clamps the model-facing envelope anyway.
pub const MAX_MCP_TOOL_TEXT_CHARS: usize = 16_000;
/// How many notification frames a request may skip before the
/// client treats the server as flooding and poisons the connection. A
/// well-behaved server never emits a flood of notifications ahead of one
/// response.
pub const DEFAULT_MAX_SKIPPED_FRAMES_PER_REQUEST: usize = 64;
pub const DEFAULT_MAX_SKIPPED_BYTES_PER_REQUEST: u64 = 1024 * 1024;
/// MCP peer cancel notification. Not Core `OperationCancelAck`.
pub const MCP_CANCEL_NOTIFICATION: &str = "notifications/cancelled";

/// A declared MCP server: identity plus how to spawn it. The id follows the
/// capability id grammar (it is embedded in catalog routes).
#[derive(Debug, Clone)]
pub struct McpServerDecl {
    pub id: String,
    pub version: String,
    pub name: String,
    pub summary: String,
    /// Program to spawn (stdio transport).
    pub program: String,
    pub args: Vec<String>,
    /// Declared permissions, from the known-word table.
    pub permissions: Vec<String>,
    /// Extra landlock write roots on Linux. Production composition leaves
    /// this empty so the server may mutate only its private cwd. Tests that
    /// observe an out-of-cwd heartbeat must name that directory explicitly;
    /// connecting never adds `/tmp` or the parent workspace on its own.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub extra_write_roots: Vec<std::path::PathBuf>,
}

/// One tool discovered from `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The text result of one `tools/call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallResult {
    /// Concatenated text content (bounded by `MAX_MCP_TOOL_TEXT_CHARS`).
    pub text: String,
    /// `result.isError` from the server.
    pub is_error: bool,
}

/// A minimal JSON-RPC 2.0 client over an arbitrary byte stream (MCP's
/// stdio transport). Generic over the reader/writer so the protocol is
/// unit-testable against in-memory duplex streams without a real process.
///
/// Frames are bounded in both directions (the shared `agent-process` frame
/// codec), and a stdio client owns its server child through
/// [`ProcessSupervisor`]: a poisoned, timed-out or cancelled exchange kills
/// and reaps the whole process tree, so late output can never be admitted
/// and a dropped connection never orphans the server.
pub struct McpClient<R, W> {
    reader: BufReader<R>,
    writer: BufWriter<W>,
    request_timeout: Duration,
    max_frame_bytes: u64,
    /// Bound on frames skipped while waiting for the matching response (a
    /// server that floods notifications is poisoned instead of
    /// being read forever).
    max_skipped_frames: usize,
    /// Cumulative notification bytes accepted before one response. A count
    /// limit alone would still permit hundreds of individually large frames;
    /// use one frame budget for the whole skipped prefix as well.
    max_skipped_bytes: u64,
    /// Stdio transports own the server through the shared supervisor.
    /// In-memory duplex tests leave this `None`.
    supervisor: Option<ProcessSupervisor>,
    /// `Some(reason)` once the connection is unusable. Poisoned clients
    /// reject every further call; the adapter replaces them on the next
    /// invoke.
    poisoned: Option<String>,
    /// Owned by the client so the child's private cwd outlives the spawn
    /// call (dropped when the connection is torn down).
    _private_cwd: Option<tempfile::TempDir>,
}

/// The concrete stdio client type produced by [`McpClient::connect_stdio`].
pub type McpStdioClient = McpClient<tokio::process::ChildStdout, tokio::process::ChildStdin>;

/// Environment keys the MCP server child inherits from the parent (plus
/// explicit `env` grants): no API keys, no HOME, no credentials.
const MCP_ENV_KEYS: &[&str] = &["PATH", "SystemRoot", "SystemDrive", "TEMP", "TMP"];

impl<R, W> McpClient<R, W>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap an existing byte stream. Callers run [`Self::initialize`]
    /// before issuing tool calls. The stream variant owns no child (see
    /// [`Self::connect_stdio`]).
    pub fn new(reader: R, writer: W, request_timeout: Duration, max_frame_bytes: u64) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            request_timeout,
            max_frame_bytes,
            max_skipped_frames: DEFAULT_MAX_SKIPPED_FRAMES_PER_REQUEST,
            max_skipped_bytes: max_frame_bytes.min(DEFAULT_MAX_SKIPPED_BYTES_PER_REQUEST),
            supervisor: None,
            poisoned: None,
            _private_cwd: None,
        }
    }

    /// True once the connection is unusable. The adapter may replace a
    /// poisoned client on the next invoke within the restart budget;
    /// exhaustion keeps this client so a later invoke cannot look like a
    /// first connect.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// Mark the connection poisoned, kill the owned server tree and return
    /// the caller-facing error. Killing happens synchronously so every
    /// exit path (parse failure, oversize frame, timeout, cancel, flood)
    /// terminates the child before the error is surfaced; callers that can
    /// await follow up with [`Self::reap`].
    fn poison(&mut self, reason: String) -> AgentError {
        self.poisoned = Some(reason.clone());
        if let Some(supervisor) = &self.supervisor {
            supervisor.kill_tree();
        }
        AgentError::Tool(format!("MCP connection poisoned: {reason}"))
    }

    /// Reap the owned child after a kill (avoids a zombie on Unix). Safe to
    /// call on a stream variant (no child): a no-op. Dropping the supervisor
    /// after reap cannot kill a reused pid: [`ProcessSupervisor::reap`]
    /// clears it first.
    async fn reap(&mut self) {
        if let Some(supervisor) = &self.supervisor {
            supervisor.reap().await;
        }
        self.supervisor = None;
    }

    /// The MCP `initialize` handshake: send the initialize request, check
    /// the protocol version in the response, then send the
    /// `notifications/initialized` notification.
    pub async fn initialize(&mut self) -> AgentResult<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "context-agent", "version": "0.1.0"}
                }),
            )
            .await?;
        let server_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if server_version != MCP_PROTOCOL_VERSION {
            return Err(self.poison(format!(
                "server protocol version '{server_version}' is not supported (expected {MCP_PROTOCOL_VERSION})"
            )));
        }
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// `tools/list`: the server's declared tools and schemas.
    pub async fn list_tools(&mut self) -> AgentResult<Vec<McpTool>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| AgentError::Tool("MCP tools/list returned no tools array".into()))?;
        tools
            .iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AgentError::Tool("MCP tool missing name".into()))?;
                let description = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input_schema = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"}));
                Ok(McpTool {
                    name: name.to_string(),
                    description,
                    input_schema,
                })
            })
            .collect()
    }

    /// `tools/call`: invoke one tool with its arguments and return the
    /// concatenated text content, bounded.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> AgentResult<McpCallResult> {
        let cancel = CancellationToken::new();
        self.call_tool_with_cancel(name, arguments, &cancel).await
    }

    /// `tools/call` that also aborts when `cancel` fires. A cancelled
    /// exchange poisons the connection and kills the owned server tree
    /// before the error surfaces, so the server's late response can never
    /// be admitted as the answer to a later call.
    pub async fn call_tool_with_cancel(
        &mut self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> AgentResult<McpCallResult> {
        let result = self
            .request_with_cancel(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                cancel,
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = bounded_tool_text(result.get("content").and_then(Value::as_array));
        Ok(McpCallResult { text, is_error })
    }

    /// Send one request and read the matching response, enforcing the
    /// request timeout and the frame bound. Notifications (no `id`) from
    /// the server are skipped until the matching id arrives; a flood of
    /// skipped frames poisons the connection.
    async fn request(&mut self, method: &str, params: Value) -> AgentResult<Value> {
        let cancel = CancellationToken::new();
        self.request_with_cancel(method, params, &cancel).await
    }

    /// Send one request and read the matching response, also aborting when
    /// `cancel` fires. Every failure path — timeout, cancellation, framing
    /// violation, flood — poisons the connection and kills the owned server
    /// tree, so a late or half-read response can never corrupt a later
    /// exchange.
    async fn request_with_cancel(
        &mut self,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> AgentResult<Value> {
        if self.is_poisoned() {
            return Err(AgentError::Tool(format!(
                "MCP connection poisoned: {}",
                self.poisoned.as_deref().unwrap_or("unknown")
            )));
        }
        if cancel.is_cancelled() {
            // Nothing was written; keep a healthy resident server.
            return Err(AgentError::Cancelled);
        }
        // A fresh v4 UUID for every exchange prevents a peer from predicting
        // the next id and pre-sending a response that a later request could
        // accidentally accept. `OperationId` is the contracts-owned UUID
        // generator; the MCP id remains an opaque JSON-RPC string.
        let id = OperationId::new().to_string();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        self.send_frame(&request).await?;

        let request_timeout = self.request_timeout;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = self
                    .send_frame(&json!({
                        "jsonrpc": "2.0",
                        "method": MCP_CANCEL_NOTIFICATION,
                        "params": { "requestId": id },
                    }))
                    .await;
                // Implicit ACK: a matching response, discarded. Silent peers
                // cannot stall past DEFAULT_CANCEL_ACK_TIMEOUT.
                let _ = tokio::time::timeout(
                    DEFAULT_CANCEL_ACK_TIMEOUT,
                    self.read_matching(&id),
                )
                .await;
                self.poison(format!("request '{method}' cancelled by the runtime"));
                self.reap().await;
                Err(AgentError::Cancelled)
            }
            result = tokio::time::timeout(request_timeout, self.read_matching(&id)) => {
                match result {
                    Ok(inner) => {
                        if inner.is_err() && self.is_poisoned() {
                            self.reap().await;
                        }
                        inner
                    }
                    Err(_) => {
                        let error = self.poison(format!("request '{method}' timed out"));
                        self.reap().await;
                        Err(error)
                    }
                }
            }
        }
    }

    /// Send a notification (a JSON-RPC message with no `id`).
    async fn notify(&mut self, method: &str, params: Value) -> AgentResult<()> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match tokio::time::timeout(self.request_timeout, self.send_frame(&frame)).await {
            Ok(result) => result,
            Err(_) => Err(self.poison(format!("notification '{method}' timed out"))),
        }
    }

    /// Write one frame with the outbound bound: an over-cap frame is
    /// rejected before a byte reaches the pipe (the connection stays
    /// usable — nothing was written).
    async fn send_frame(&mut self, frame: &Value) -> AgentResult<()> {
        let limit = usize::try_from(self.max_frame_bytes).map_err(|_| {
            AgentError::InvalidRequest(format!(
                "MCP frame bound {} does not fit this platform",
                self.max_frame_bytes
            ))
        })?;
        // Encoding (including the size check) happens before any write. An
        // oversize caller value therefore leaves the connection usable.
        let line = agent_process::encode_frame(frame, limit)?;
        if let Err(error) = self.writer.write_all(&line).await {
            return Err(self.poison(format!("write frame: {error}")));
        }
        if let Err(error) = self.writer.flush().await {
            return Err(self.poison(format!("flush frame: {error}")));
        }
        Ok(())
    }

    /// Read frames until the one carrying `id` arrives; notifications and
    /// notifications are skipped, up to `max_skipped_frames`. A response
    /// carrying any other id is a protocol violation under single-inflight:
    /// it may be a late or pre-sent answer and poisons the connection.
    async fn read_matching(&mut self, id: &str) -> AgentResult<Value> {
        let mut skipped = 0usize;
        let mut skipped_bytes = 0u64;
        loop {
            let (frame, frame_bytes) = self.read_frame().await?;
            // Identity comes first. In particular, an error attached to a
            // different id is not the current request's error. Under the
            // single-inflight contract it is fatal, not a skippable response.
            let frame_id = frame.get("id");
            if frame_id.is_some() && frame_id.and_then(Value::as_str) != Some(id) {
                return Err(self.poison(format!(
                    "response id mismatch while waiting for request {id}"
                )));
            }
            if frame_id.is_none() {
                if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
                    || frame.get("method").and_then(Value::as_str).is_none()
                {
                    return Err(self.poison(format!(
                        "malformed notification while waiting for request {id}"
                    )));
                }
                skipped += 1;
                skipped_bytes =
                    skipped_bytes.saturating_add(u64::try_from(frame_bytes).unwrap_or(u64::MAX));
                if skipped > self.max_skipped_frames || skipped_bytes > self.max_skipped_bytes {
                    return Err(self.poison(format!(
                        "request {id}: notification flood exceeded {} frames or {} bytes",
                        self.max_skipped_frames, self.max_skipped_bytes
                    )));
                }
                continue;
            }
            if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(self.poison(format!("response {id} has an invalid JSON-RPC version")));
            }
            let result = frame.get("result");
            let error = frame.get("error");
            if result.is_some() == error.is_some() {
                return Err(self.poison(format!(
                    "response {id} must contain exactly one of result or error"
                )));
            }
            if let Some(value) = error {
                let Some(message) = value.get("message").and_then(Value::as_str) else {
                    return Err(
                        self.poison(format!("response {id} has a malformed JSON-RPC error"))
                    );
                };
                return Err(AgentError::Tool(format!(
                    "MCP request {id} failed: {message}"
                )));
            }
            return Ok(result.expect("exactly one result/error checked").clone());
        }
    }

    /// Read one newline-delimited JSON frame. The shared incremental frame
    /// codec enforces the bound while reading (an over-cap line is rejected
    /// instead of buffered in full); every framing failure poisons the
    /// connection so the half-read stream is never reused.
    async fn read_frame(&mut self) -> AgentResult<(Value, usize)> {
        if self.is_poisoned() {
            return Err(AgentError::Tool(format!(
                "MCP connection poisoned: {}",
                self.poisoned.as_deref().unwrap_or("unknown")
            )));
        }
        let limit = usize::try_from(self.max_frame_bytes).map_err(|_| {
            AgentError::InvalidRequest(format!(
                "MCP frame bound {} does not fit this platform",
                self.max_frame_bytes
            ))
        })?;
        let frame = match agent_process::read_frame(&mut self.reader, limit).await {
            Ok(frame) => frame,
            Err(error) => {
                return Err(self.poison(error.to_string()));
            }
        };
        let frame_bytes = frame.len();
        // 编码帧已封顶；这里再挡 `[{},…]` 一类解码 DOM 放大。
        let budget = JsonDecodeBudget::for_frame_bytes(limit);
        match decode_value(&frame, &budget) {
            Ok(value) => Ok((value, frame_bytes)),
            Err(error) => Err(self.poison(format!("parse MCP frame: {error}"))),
        }
    }
}

impl McpClient<tokio::process::ChildStdout, tokio::process::ChildStdin> {
    /// Spawn the declared server over stdio with a scrubbed environment and
    /// a private cwd, then run the MCP `initialize` handshake. The child is
    /// owned through [`ProcessSupervisor`]: teardown, timeout, cancellation
    /// and framing failures kill and reap the whole process tree, so a dead
    /// or abandoned server never keeps running and late output can never be
    /// admitted. The child's stderr is discarded (the runtime's process host
    /// owns stderr accounting for its own protocol; MCP servers log to
    /// stderr freely).
    pub async fn connect_stdio(
        decl: &McpServerDecl,
        request_timeout: Duration,
        max_frame_bytes: u64,
    ) -> AgentResult<Self> {
        validate_capability_id(&decl.id).map_err(AgentError::InvalidRequest)?;
        let private = tempfile::tempdir()
            .map_err(|e| AgentError::Tool(format!("create MCP private cwd: {e}")))?;
        let mut command = tokio::process::Command::new(&decl.program);
        command
            .args(&decl.args)
            .current_dir(private.path())
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            // The child dies with its owning handle even when every explicit
            // teardown path is skipped (e.g. the adapter is dropped).
            .kill_on_drop(true);
        for key in MCP_ENV_KEYS {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        {
            let mut roots = vec![private.path().to_path_buf()];
            roots.extend(decl.extra_write_roots.iter().cloned());
            agent_process::integrity::label_write_roots(&roots).map_err(|e| {
                AgentError::Tool(format!("integrity sandbox setup for MCP server: {e}"))
            })?;
            let (program, args) = agent_process::integrity::wrap_command(&decl.program, &decl.args)
                .map_err(|e| {
                    AgentError::Tool(format!("spawn MCP server '{}': {e}", decl.program))
                })?;
            command = tokio::process::Command::new(&program);
            command
                .args(&args)
                .current_dir(private.path())
                .env_clear()
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            for key in MCP_ENV_KEYS {
                if let Ok(value) = std::env::var(key) {
                    command.env(key, value);
                }
            }
        }
        // Unix rlimits and Linux landlock share one pre_exec so a
        // toolchain that keeps only the last hook cannot drop the memory,
        // file-size or open-file ceiling when write roots are configured.
        // RLIMIT_AS is 2 GiB VAS, RLIMIT_FSIZE is 256 MiB and
        // RLIMIT_NOFILE is 1024 (same defaults as process capabilities).
        // apply_unix_rlimits also zeros RLIMIT_CORE (`MOD-15`) and on
        // Linux clamps NICE/RTPRIO and sets no_new_privs (`MOD-16`).
        // CPU/nproc stay unset here: RLIMIT_NPROC is per-user on Linux
        // and a small cap can starve MCP handshake threads on a busy CI
        // host. Inherited fds other than stdio are closed after landlock.
        #[cfg(unix)]
        {
            const MCP_MAX_MEMORY_BYTES: u64 = 2u64 * 1024 * 1024 * 1024;
            const MCP_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
            const MCP_MAX_OPEN_FILES: u64 = 1024;
            #[cfg(target_os = "linux")]
            let landlock_rules = {
                let mut roots = vec![private.path().to_path_buf()];
                roots.extend(decl.extra_write_roots.iter().cloned());
                if !agent_process::landlock::available() {
                    eprintln!(
                        "landlock sandbox skipped: kernel support unavailable \
                         (OS-level write/TCP confinement off for MCP server '{}')",
                        decl.program
                    );
                    None
                } else {
                    Some(
                        agent_process::landlock::ChildRules::open(&roots).map_err(|e| {
                            AgentError::Tool(format!("landlock sandbox setup for MCP server: {e}"))
                        })?,
                    )
                }
            };
            unsafe {
                command.pre_exec(move || {
                    agent_process::apply_unix_rlimits(
                        0,
                        0,
                        MCP_MAX_MEMORY_BYTES,
                        MCP_MAX_FILE_BYTES,
                        MCP_MAX_OPEN_FILES,
                    )?;
                    #[cfg(target_os = "linux")]
                    if let Some(rules) = landlock_rules.as_ref() {
                        agent_process::landlock::apply_in_child(rules)?;
                    }
                    agent_process::close_inherited_fds();
                    Ok(())
                });
            }
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn MCP server '{}': {e}", decl.program)))?;
        let Some(pid) = child.id() else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(AgentError::Tool("MCP server pid not available".into()));
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                ProcessSupervisor::from_child(child, pid).terminate().await;
                return Err(AgentError::Tool("MCP server stdin not available".into()));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                ProcessSupervisor::from_child(child, pid).terminate().await;
                return Err(AgentError::Tool("MCP server stdout not available".into()));
            }
        };
        let mut client = Self::new(stdout, stdin, request_timeout, max_frame_bytes);
        client.supervisor = Some(ProcessSupervisor::from_child(child, pid));
        if let Err(error) = client.initialize().await {
            // The handshake failed: the spawned server must not be left
            // running. Kill and reap it before surfacing the error.
            client.poison("initialize handshake failed".into());
            client.reap().await;
            return Err(error);
        }
        // The private cwd is owned by the client: it lives for the child's
        // lifetime and is removed when the connection is torn down.
        client._private_cwd = Some(private);
        Ok(client)
    }
}

fn bounded_tool_text(content: Option<&Vec<Value>>) -> String {
    let Some(items) = content else {
        return String::new();
    };
    let mut text = String::new();
    let mut remaining = MAX_MCP_TOOL_TEXT_CHARS;
    let mut truncated = false;
    for item in items {
        let Some(part) = item.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !text.is_empty() {
            if remaining == 0 {
                truncated = true;
                break;
            }
            text.push('\n');
            remaining -= 1;
        }
        let count = part.chars().count();
        if count <= remaining {
            text.push_str(part);
            remaining -= count;
        } else {
            text.extend(part.chars().take(remaining));
            truncated = true;
            break;
        }
    }
    if truncated {
        text.push_str("\n... (MCP text truncated)");
    }
    text
}

/// An MCP server as a `Capability`: the tools discovered at connect time
/// enter the manifest as static schemas (and through the registry's
/// catalog, loaded on demand), and every invocation is forwarded as a
/// `tools/call` whose bounded text result becomes a `ToolOutput`. The
/// server child is started lazily on first invoke after the initial
/// discovery connection.
pub struct McpCapabilityAdapter {
    manifest: CapabilityManifest,
    decl: McpServerDecl,
    request_timeout: Duration,
    max_frame_bytes: u64,
    client: tokio::sync::Mutex<HostLifecycle<McpStdioClient>>,
    /// Replacements of a poisoned resident server. Discovery connect/reap
    /// does not count. Connection state is not task or Core authority.
    restart: RestartCircuit,
}

impl McpCapabilityAdapter {
    /// Connect once, discover the server's tools, and build the adapter
    /// with a static manifest. `risk` is the caller's classification of
    /// the server's tools (derived from declared permissions, never
    /// self-declared by the server).
    pub async fn connect(
        decl: McpServerDecl,
        risk: ToolRisk,
        request_timeout: Duration,
        max_frame_bytes: u64,
    ) -> AgentResult<Self> {
        validate_capability_id(&decl.id).map_err(AgentError::InvalidRequest)?;
        let mut client = McpClient::connect_stdio(&decl, request_timeout, max_frame_bytes).await?;
        let tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                client.poison("tool discovery failed".into());
                client.reap().await;
                return Err(error);
            }
        };
        // Discovery establishes the static manifest only. The declared
        // lifecycle is Lazy, so no server stays resident until first invoke.
        client.poison("tool discovery completed".into());
        client.reap().await;
        let tool_specs: Vec<ToolSpec> = tools
            .into_iter()
            .map(|tool| ToolSpec {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
                risk,
                output_budget: None,
                roles: Vec::new(),
            })
            .collect();
        let manifest = CapabilityManifest {
            id: decl.id.clone(),
            version: decl.version.clone(),
            name: decl.name.clone(),
            summary: decl.summary.clone(),
            status: CapabilityStatus::Experimental,
            provides: vec![CapabilityKind::Tool],
            permissions: decl.permissions.clone(),
            requires: Vec::new(),
            tools: tool_specs,
            lifecycle: CapabilityLifecycle::Lazy,
            transport: CapabilityTransport::Process {
                program: decl.program.clone(),
            },
            sandbox_profile: Default::default(),
        };
        Ok(Self {
            manifest,
            decl,
            request_timeout,
            max_frame_bytes,
            client: tokio::sync::Mutex::new(HostLifecycle::NeverStarted),
            restart: RestartCircuit::new(agent_process::DEFAULT_MAX_CONNECTION_RESTARTS),
        })
    }

    #[cfg(test)]
    fn with_restart_circuit(self, restart: RestartCircuit) -> Self {
        Self { restart, ..self }
    }
}

impl std::fmt::Debug for McpCapabilityAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCapabilityAdapter")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Capability for McpCapabilityAdapter {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    async fn invoke(
        &self,
        call: ToolCall,
        ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        if ctx.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let mut guard = self.client.lock().await;
        // Cancellation may occur while queued behind the single-inflight
        // lock; do not reconnect or write a request after that point.
        if ctx.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let needs_connect = match &*guard {
            HostLifecycle::Serving(client) => client.is_poisoned(),
            HostLifecycle::NeverStarted
            | HostLifecycle::Stopped
            | HostLifecycle::Quarantined { .. } => true,
        };
        if needs_connect {
            if guard.connect_kind() == agent_process::ConnectKind::Restart {
                self.restart.try_acquire()?;
                if let HostLifecycle::Serving(mut stale) = std::mem::replace(
                    &mut *guard,
                    HostLifecycle::Quarantined {
                        reason: "restarting".into(),
                    },
                ) {
                    stale.reap().await;
                }
            }
            match McpClient::connect_stdio(&self.decl, self.request_timeout, self.max_frame_bytes)
                .await
            {
                Ok(client) => *guard = HostLifecycle::Serving(client),
                Err(error) => {
                    guard.record_connect_failure(error.to_string());
                    return Err(error);
                }
            }
        }
        let client = guard.serving_mut().expect("client present after ensure");
        let result = client
            .call_tool_with_cancel(&call.name, call.arguments, &ctx.cancel)
            .await?;
        let output = ToolOutput {
            call_id: call.id,
            tool_name: call.name,
            ok: !result.is_error,
            summary: if result.is_error {
                "MCP tool reported an error".to_string()
            } else {
                "MCP tool call completed".to_string()
            },
            model_content: result.text,
            artifact_ref: None,
            metadata: json!({"mcp": true, "is_error": result.is_error}),
        };
        Ok(CapabilityOutcome::Value(output))
    }

    /// Teardown: the owned server child is killed and reaped, so host stop
    /// never leaves the server process behind.
    async fn stop(&self) -> AgentResult<()> {
        let mut guard = self.client.lock().await;
        if let HostLifecycle::Serving(mut client) =
            std::mem::replace(&mut *guard, HostLifecycle::Stopped)
        {
            client.poison("capability stopped by the runtime".into());
            client.reap().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, duplex};

    /// Read one newline-terminated line from a raw stream (duplex streams
    /// are not buffered, so `read_until` needs a hand-rolled loop here).
    async fn read_line(
        read: &mut (impl AsyncRead + Unpin),
        buf: &mut Vec<u8>,
    ) -> std::io::Result<usize> {
        let mut byte = [0u8; 1];
        loop {
            let count = read.read(&mut byte).await?;
            if count == 0 {
                return Ok(0);
            }
            buf.push(byte[0]);
            if byte[0] == b'\n' {
                return Ok(buf.len());
            }
        }
    }

    /// A tiny in-memory MCP server speaking JSON-RPC over a duplex stream.
    /// Responds to initialize/tools/list/tools/call, echoes otherwise.
    async fn mock_server(
        mut read: impl AsyncRead + Unpin + Send,
        mut write: impl AsyncWrite + Unpin + Send,
    ) {
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(read_count) = read_line(&mut read, &mut line).await else {
                return;
            };
            if read_count == 0 {
                return;
            }
            let Ok(request) = serde_json::from_slice::<Value>(&line) else {
                return;
            };
            let method = request.get("method").and_then(Value::as_str).unwrap_or("");
            let id = request.get("id");
            if id.is_none() {
                continue; // notification: no response
            }
            let response = match method {
                "initialize" => json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"protocolVersion": MCP_PROTOCOL_VERSION, "capabilities": {}, "serverInfo": {"name": "mock", "version": "0.1.0"}}
                }),
                "tools/list" => json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": [
                        {"name": "mock.add", "description": "add two numbers", "inputSchema": {"type": "object"}},
                        {"name": "mock.echo", "description": "echo text", "inputSchema": {"type": "object"}}
                    ]}
                }),
                "tools/call" => {
                    let name = request["params"]["name"].as_str().unwrap_or("");
                    let arguments = &request["params"]["arguments"];
                    match name {
                        "mock.echo" => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": arguments["text"].as_str().unwrap_or("")}]}
                        }),
                        "mock.fail" => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": "boom"}], "isError": true}
                        }),
                        _ => json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": "method not found"}
                        }),
                    }
                }
                _ => json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                }),
            };
            let mut frame = serde_json::to_string(&response).unwrap();
            frame.push('\n');
            if write.write_all(frame.as_bytes()).await.is_err() {
                return;
            }
            let _ = write.flush().await;
        }
    }

    async fn duplex_client() -> McpClient<tokio::io::DuplexStream, tokio::io::DuplexStream> {
        let (client_read, server_write) = duplex(64 * 1024);
        let (server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            mock_server(server_read, server_write).await;
        });
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            1024 * 1024,
        );
        client.initialize().await.expect("handshake succeeds");
        client
    }

    #[tokio::test]
    async fn cancel_sends_notifications_cancelled_then_poisons() {
        let (client_read, mut server_write) = duplex(64 * 1024);
        let (mut server_read, client_write) = duplex(64 * 1024);
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            loop {
                let mut line = Vec::new();
                let Ok(count) = read_line(&mut server_read, &mut line).await else {
                    return;
                };
                if count == 0 {
                    return;
                }
                let Ok(request) = serde_json::from_slice::<Value>(&line) else {
                    return;
                };
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                if method == MCP_CANCEL_NOTIFICATION {
                    let _ = seen_tx.send(method.to_string());
                    std::future::pending::<()>().await;
                    return;
                }
                let id = request.get("id");
                if id.is_none() {
                    continue;
                }
                if method == "tools/call" {
                    continue;
                }
                let response = match method {
                    "initialize" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": MCP_PROTOCOL_VERSION,
                            "capabilities": {},
                            "serverInfo": {"name": "mock", "version": "0.1.0"}
                        }
                    }),
                    _ => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    }),
                };
                let mut frame = serde_json::to_string(&response).unwrap();
                frame.push('\n');
                if server_write.write_all(frame.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            1024 * 1024,
        );
        client.initialize().await.expect("handshake succeeds");
        let cancel = CancellationToken::new();
        let fire = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            fire.cancel();
        });
        let error = client
            .call_tool_with_cancel("mock.echo", json!({"text": "hang"}), &cancel)
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::Cancelled),
            "MCP cancel must surface as Cancelled: {error}"
        );
        assert!(client.is_poisoned());
        let method = tokio::time::timeout(Duration::from_secs(2), seen_rx)
            .await
            .expect("cancel notification observed")
            .expect("oneshot");
        assert_eq!(method, MCP_CANCEL_NOTIFICATION);
    }

    #[tokio::test]
    async fn client_handshakes_and_lists_tools() {
        let mut client = duplex_client().await;
        let tools = client.list_tools().await.expect("tools/list succeeds");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "mock.add");
        assert_eq!(tools[1].name, "mock.echo");
        assert!(tools[0].input_schema.get("type").is_some());
    }

    #[tokio::test]
    async fn client_calls_tools_and_reports_errors() {
        let mut client = duplex_client().await;
        let result = client
            .call_tool("mock.echo", json!({"text": "hello mcp"}))
            .await
            .expect("call succeeds");
        assert!(!result.is_error);
        assert_eq!(result.text, "hello mcp");

        let failed = client
            .call_tool("mock.fail", json!({}))
            .await
            .expect("isError results are not transport errors");
        assert!(failed.is_error);
        assert_eq!(failed.text, "boom");

        let unknown = client.call_tool("nope", json!({})).await;
        assert!(unknown.is_err(), "a JSON-RPC error must surface");
        assert!(
            unknown
                .unwrap_err()
                .to_string()
                .contains("method not found"),
            "the error message must survive"
        );
    }

    #[tokio::test]
    async fn client_handles_notifications_between_responses() {
        // The server sends a notification right before the real response;
        // the client must skip it and match by id.
        let (client_read, mut server_write) = duplex(64 * 1024);
        let (mut server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            loop {
                let mut line = Vec::new();
                let Ok(count) = read_line(&mut server_read, &mut line).await else {
                    return;
                };
                if count == 0 {
                    return;
                }
                let Ok(request) = serde_json::from_slice::<Value>(&line) else {
                    return;
                };
                let id = request.get("id");
                if id.is_none() {
                    continue;
                }
                // A notification (no id) ahead of the real response.
                let _ = server_write
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"server/notification\"}\n")
                    .await;
                let response = json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content": [{"type": "text", "text": "after notification"}]}
                });
                let mut frame = serde_json::to_string(&response).unwrap();
                frame.push('\n');
                let _ = server_write.write_all(frame.as_bytes()).await;
                let _ = server_write.flush().await;
            }
        });
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            1024 * 1024,
        );
        // Skip the real handshake protocol details: drive calls directly.
        let result = client
            .call_tool("mock.echo", json!({"text": "x"}))
            .await
            .expect("notification is skipped");
        assert_eq!(result.text, "after notification");
    }

    #[tokio::test]
    async fn client_read_timeout_is_a_clean_error() {
        let (client_read, _server_write) = duplex(64 * 1024);
        let (_server_read, client_write) = duplex(64 * 1024);
        // No server task: the request must time out, not hang.
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_millis(200),
            1024 * 1024,
        );
        let result = client.call_tool("mock.echo", json!({})).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("timed out"),
            "timeout must be reported"
        );
    }

    #[tokio::test]
    async fn notification_flood_is_bounded_and_poisons_the_client() {
        // A server that streams notifications forever instead of answering:
        // the client must stop after the skip bound and poison itself, so a
        // flooding server is never read forever and a later call cannot
        // mistake a stale frame for its answer.
        let (client_read, mut server_write) = duplex(64 * 1024);
        let (_server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            let flood = DEFAULT_MAX_SKIPPED_FRAMES_PER_REQUEST + 10;
            for _ in 0..flood {
                let _ = server_write
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"server/notification\"}\n")
                    .await;
            }
            let _ = server_write.flush().await;
            // Never answer the request; the client must give up on its own.
            std::future::pending::<()>().await;
        });
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            1024 * 1024,
        );
        let result = client.call_tool("mock.echo", json!({})).await;
        assert!(
            result.is_err(),
            "a notification flood must not be read forever"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("notification flood"),
            "the flood must be named in the error"
        );
        assert!(client.is_poisoned(), "the flooded client must be poisoned");

        let second = client.call_tool("mock.echo", json!({})).await;
        assert!(
            second.is_err(),
            "a poisoned client rejects further calls immediately"
        );
    }

    #[tokio::test]
    async fn decoded_json_node_budget_poisons_the_client() {
        let (client_read, mut server_write) = duplex(512 * 1024);
        let (_server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            let mut frame = Vec::from(&b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":["[..]);
            for index in 0..70_000 {
                if index > 0 {
                    frame.push(b',');
                }
                frame.extend_from_slice(b"{}");
            }
            frame.extend_from_slice(b"]}\n");
            let _ = server_write.write_all(&frame).await;
            let _ = server_write.flush().await;
        });
        let mut client = McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            1024 * 1024,
        );
        let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
        assert!(
            error.to_string().contains("json decode budget"),
            "a frame-legal empty-object array must fail the decoded node budget, got: {error}"
        );
        assert!(client.is_poisoned());
    }

    #[tokio::test]
    async fn notification_bytes_are_bounded_even_below_the_count_limit() {
        let (client_read, mut server_write) = duplex(64 * 1024);
        let (_server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            // Four sub-frame notifications exceed the cumulative 1 KiB
            // skipped-byte budget while staying far below the count bound.
            let frame = format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"notice\",\"params\":{{\"data\":\"{}\"}}}}\n",
                "x".repeat(300)
            );
            for _ in 0..4 {
                let _ = server_write.write_all(frame.as_bytes()).await;
            }
            let _ = server_write.flush().await;
            std::future::pending::<()>().await;
        });
        let mut client = McpClient::new(client_read, client_write, Duration::from_secs(5), 1024);
        let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
        assert!(error.to_string().contains("notification flood"));
        assert!(client.is_poisoned());
    }

    #[tokio::test]
    async fn outbound_oversize_writes_nothing_and_connection_remains_usable() {
        let (client_read, server_write) = duplex(64 * 1024);
        let (server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            mock_server(server_read, server_write).await;
        });
        let mut client = McpClient::new(client_read, client_write, Duration::from_secs(5), 512);
        let error = client
            .call_tool("mock.echo", json!({"text": "x".repeat(1024)}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nothing was written"));
        assert!(!client.is_poisoned());

        let result = client
            .call_tool("mock.echo", json!({"text": "small"}))
            .await
            .expect("pre-write rejection leaves framing intact");
        assert_eq!(result.text, "small");
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected_while_reading_and_poisons() {
        // A single over-cap line: the incremental frame reader must reject
        // it while reading (never buffer it in full) and poison the client.
        let (client_read, mut server_write) = duplex(64 * 1024);
        let (_server_read, client_write) = duplex(64 * 1024);
        tokio::spawn(async move {
            let mut line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"".to_vec();
            line.extend(std::iter::repeat_n(b'x', 2 * 1024 * 1024));
            line.extend_from_slice(b"\"}]}}\n");
            let _ = server_write.write_all(&line).await;
            let _ = server_write.flush().await;
            std::future::pending::<()>().await;
        });
        let mut client =
            McpClient::new(client_read, client_write, Duration::from_secs(5), 64 * 1024);
        let result = client.call_tool("mock.echo", json!({})).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("byte limit"),
            "oversize must be reported as a frame-bound violation"
        );
        assert!(client.is_poisoned(), "the client must be poisoned");

        let second = client.call_tool("mock.echo", json!({})).await;
        assert!(
            second.is_err(),
            "a poisoned client rejects further calls immediately"
        );
    }

    #[test]
    fn text_clipping_is_bounded() {
        let long = "x".repeat(MAX_MCP_TOOL_TEXT_CHARS + 500);
        let content = vec![json!({"type": "text", "text": long})];
        let clipped = bounded_tool_text(Some(&content));
        assert!(
            clipped.chars().count() <= MAX_MCP_TOOL_TEXT_CHARS + 64,
            "clip must stay bounded"
        );
        assert!(clipped.contains("truncated"));
    }

    /// Locate the `mcp_mock_server` bin built next to the test binaries.
    fn locate_mock_server() -> Option<std::path::PathBuf> {
        let name = if cfg!(windows) {
            "mcp_mock_server.exe"
        } else {
            "mcp_mock_server"
        };
        let current = std::env::current_exe().ok()?;
        agent_process::probe_siblings(&current, name)
    }

    fn mock_decl() -> McpServerDecl {
        McpServerDecl {
            id: "mock-mcp".into(),
            version: "1.0.0".into(),
            name: "mock mcp".into(),
            summary: "mock server for tests".into(),
            program: locate_mock_server()
                .expect("mcp_mock_server built")
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
            permissions: vec!["workspace:read".into()],
            extra_write_roots: Vec::new(),
        }
    }

    #[tokio::test]
    async fn adapter_discovers_tools_and_invokes_them() {
        let adapter = McpCapabilityAdapter::connect(
            mock_decl(),
            ToolRisk::ReadOnly,
            Duration::from_secs(10),
            1024 * 1024,
        )
        .await
        .expect("connect + discover succeeds");

        let manifest = adapter.manifest();
        assert_eq!(manifest.id, "mock-mcp");
        assert_eq!(
            manifest.tools.len(),
            3,
            "tools/list must populate the manifest"
        );
        assert!(manifest.tools.iter().any(|t| t.name == "mock.echo"));
        assert!(manifest.tools.iter().any(|t| t.name == "mock.add"));
        assert!(manifest.tools.iter().any(|t| t.name == "mock.fail"));
        assert!(
            matches!(&*adapter.client.lock().await, HostLifecycle::NeverStarted),
            "lazy discovery must reap its temporary server"
        );

        // Invoke mock.echo through the Capability boundary.
        let call = ToolCall {
            id: "c1".into(),
            name: "mock.echo".into(),
            arguments: json!({"text": "hello from the model"}),
        };
        let outcome = adapter
            .invoke(
                call,
                CapabilityInvocationContext {
                    granted_permissions: Vec::new(),
                    workspace: None,
                    artifacts: None,
                    approved_intent: None,
                    cancel: agent_contracts::CancellationToken::new(),
                },
            )
            .await
            .expect("invoke succeeds");
        assert!(
            adapter.client.lock().await.serving().is_some(),
            "first invoke must establish the lazy execution session"
        );
        let CapabilityOutcome::Value(output) = outcome else {
            panic!("mock.echo must return a plain value");
        };
        assert!(output.ok);
        assert_eq!(output.model_content, "hello from the model");
        assert_eq!(output.tool_name, "mock.echo");

        // A server-declared error surfaces as ok:false, not a transport error.
        let call = ToolCall {
            id: "c2".into(),
            name: "mock.fail".into(),
            arguments: json!({}),
        };
        let outcome = adapter
            .invoke(
                call,
                CapabilityInvocationContext {
                    granted_permissions: Vec::new(),
                    workspace: None,
                    artifacts: None,
                    approved_intent: None,
                    cancel: agent_contracts::CancellationToken::new(),
                },
            )
            .await
            .expect("invoke succeeds");
        let CapabilityOutcome::Value(output) = outcome else {
            panic!("mock.fail must return a plain value");
        };
        assert!(
            !output.ok,
            "a server isError must reach the model as ok:false"
        );
    }

    #[tokio::test]
    async fn adapter_exhausted_restart_budget_keeps_the_poisoned_client() {
        let adapter = McpCapabilityAdapter::connect(
            mock_decl(),
            ToolRisk::ReadOnly,
            Duration::from_secs(10),
            1024 * 1024,
        )
        .await
        .expect("connect + discover succeeds")
        .with_restart_circuit(RestartCircuit::new(0));
        let invoke_ctx = || CapabilityInvocationContext {
            granted_permissions: Vec::new(),
            workspace: None,
            artifacts: None,
            approved_intent: None,
            cancel: agent_contracts::CancellationToken::new(),
        };
        let first = ToolCall {
            id: "c1".into(),
            name: "mock.echo".into(),
            arguments: json!({"text": "once"}),
        };
        adapter
            .invoke(first, invoke_ctx())
            .await
            .expect("first connect is not a restart");
        {
            let mut guard = adapter.client.lock().await;
            guard
                .serving_mut()
                .expect("resident client")
                .poison("test fence".into());
        }
        let second = ToolCall {
            id: "c2".into(),
            name: "mock.echo".into(),
            arguments: json!({"text": "again"}),
        };
        let error = adapter.invoke(second, invoke_ctx()).await.unwrap_err();
        assert!(
            error.to_string().contains("restart budget exhausted"),
            "a zero restart budget must not spawn a replacement: {error}"
        );
        assert!(
            adapter
                .client
                .lock()
                .await
                .serving()
                .is_some_and(|client| client.is_poisoned()),
            "exhausted restart must keep the poisoned client"
        );
    }

    #[tokio::test]
    async fn adapter_connect_failure_is_a_clean_error() {
        let mut decl = mock_decl();
        decl.program = "definitely-not-a-real-program-xyz".into();
        let error = McpCapabilityAdapter::connect(
            decl,
            ToolRisk::ReadOnly,
            Duration::from_secs(2),
            1024 * 1024,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("spawn"),
            "a missing server must be a clean spawn error: {error}"
        );
    }

    #[tokio::test]
    async fn adapter_results_are_bounded() {
        let adapter = McpCapabilityAdapter::connect(
            mock_decl(),
            ToolRisk::ReadOnly,
            Duration::from_secs(10),
            1024 * 1024,
        )
        .await
        .expect("connect succeeds");
        // The mock echoes whatever text it receives; a huge echo must come
        // back clipped to the MCP text bound.
        let big = "x".repeat(MAX_MCP_TOOL_TEXT_CHARS + 1_000);
        let call = ToolCall {
            id: "c3".into(),
            name: "mock.echo".into(),
            arguments: json!({"text": big}),
        };
        let outcome = adapter
            .invoke(
                call,
                CapabilityInvocationContext {
                    granted_permissions: Vec::new(),
                    workspace: None,
                    artifacts: None,
                    approved_intent: None,
                    cancel: agent_contracts::CancellationToken::new(),
                },
            )
            .await
            .expect("invoke succeeds");
        let CapabilityOutcome::Value(output) = outcome else {
            panic!("mock.echo must return a plain value");
        };
        assert!(
            output.model_content.chars().count() <= MAX_MCP_TOOL_TEXT_CHARS + 64,
            "MCP text must be clipped before it reaches the model"
        );
    }
}

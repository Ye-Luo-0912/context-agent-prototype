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
    AgentError, AgentResult, Capability, CapabilityInvocationContext, CapabilityKind,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ToolCall, ToolOutput, ToolRisk, ToolSpec, validate_capability_id,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

/// MCP protocol version this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Default per-request timeout for MCP calls.
pub const DEFAULT_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default bound on one stdio frame (a JSON-RPC document).
pub const DEFAULT_MCP_MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
/// Bounds on one tool's text content (chars) after concatenation; the
/// kernel output broker clamps the model-facing envelope anyway.
pub const MAX_MCP_TOOL_TEXT_CHARS: usize = 16_000;

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
pub struct McpClient<R, W> {
    reader: BufReader<R>,
    writer: BufWriter<W>,
    next_id: u64,
    request_timeout: Duration,
    max_frame_bytes: u64,
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
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap an existing byte stream. Callers run [`Self::initialize`]
    /// before issuing tool calls.
    pub fn new(reader: R, writer: W, request_timeout: Duration, max_frame_bytes: u64) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer: BufWriter::new(writer),
            next_id: 1,
            request_timeout,
            max_frame_bytes,
            _private_cwd: None,
        }
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
            return Err(AgentError::Tool(format!(
                "MCP server protocol version '{server_version}' is not supported (expected {MCP_PROTOCOL_VERSION})"
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
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = result.get("content").and_then(Value::as_array);
        let text: String = content
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let text = clip_text(&text);
        Ok(McpCallResult { text, is_error })
    }

    /// Send one request and read the matching response, enforcing the
    /// request timeout and the frame bound. Notifications (no `id`) from
    /// the server are skipped until the matching id arrives.
    async fn request(&mut self, method: &str, params: Value) -> AgentResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_frame(&request).await?;
        tokio::time::timeout(self.request_timeout, self.read_matching(id))
            .await
            .map_err(|_| AgentError::Tool(format!("MCP request '{method}' timed out")))?
    }

    /// Send a notification (a JSON-RPC message with no `id`).
    async fn notify(&mut self, method: &str, params: Value) -> AgentResult<()> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_frame(&frame).await
    }

    async fn send_frame(&mut self, frame: &Value) -> AgentResult<()> {
        let mut line = serde_json::to_string(frame)
            .map_err(|e| AgentError::Tool(format!("serialize MCP frame: {e}")))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| AgentError::Tool(format!("write MCP frame: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| AgentError::Tool(format!("flush MCP frame: {e}")))?;
        Ok(())
    }

    /// Read frames until the one carrying `id` arrives; notifications are
    /// skipped. Errors (a JSON-RPC error object) surface as `AgentError`.
    async fn read_matching(&mut self, id: u64) -> AgentResult<Value> {
        loop {
            let frame = self.read_frame().await?;
            let frame_id = frame.get("id");
            if let Some(value) = frame.get("error") {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown MCP error")
                    .to_string();
                return Err(AgentError::Tool(format!(
                    "MCP request {id} failed: {message}"
                )));
            }
            if frame_id.and_then(Value::as_u64) == Some(id) {
                return frame
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AgentError::Tool(format!("MCP response {id} has no result")));
            }
            // A notification or a stale id: skip and keep reading.
        }
    }

    /// Read one newline-delimited JSON frame, enforcing the frame bound.
    async fn read_frame(&mut self) -> AgentResult<Value> {
        let mut line = Vec::new();
        let read = tokio::time::timeout(self.request_timeout, async {
            self.reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| AgentError::Tool(format!("read MCP frame: {e}")))
        })
        .await
        .map_err(|_| AgentError::Tool("MCP read timed out".into()))??;
        if read == 0 {
            return Err(AgentError::Tool("MCP server closed the connection".into()));
        }
        if line.len() as u64 > self.max_frame_bytes {
            return Err(AgentError::Tool(format!(
                "MCP frame is {} bytes, above the {}-byte bound",
                line.len(),
                self.max_frame_bytes
            )));
        }
        serde_json::from_slice(&line).map_err(|e| AgentError::Tool(format!("parse MCP frame: {e}")))
    }
}

impl McpClient<tokio::process::ChildStdout, tokio::process::ChildStdin> {
    /// Spawn the declared server over stdio with a scrubbed environment and
    /// a private cwd, then run the MCP `initialize` handshake. The child's
    /// stderr is discarded (the runtime's process host owns stderr
    /// accounting for its own protocol; MCP servers log to stderr freely).
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
            .stderr(std::process::Stdio::null());
        for key in MCP_ENV_KEYS {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| AgentError::Tool(format!("spawn MCP server '{}': {e}", decl.program)))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Tool("MCP server stdin not available".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Tool("MCP server stdout not available".into()))?;
        let mut client = Self::new(stdout, stdin, request_timeout, max_frame_bytes);
        client.initialize().await?;
        // The private cwd is owned by the client: it lives for the child's
        // lifetime and is removed when the connection is torn down.
        client._private_cwd = Some(private);
        Ok(client)
    }
}

fn clip_text(text: &str) -> String {
    if text.chars().count() <= MAX_MCP_TOOL_TEXT_CHARS {
        return text.to_string();
    }
    let clipped: String = text.chars().take(MAX_MCP_TOOL_TEXT_CHARS).collect();
    format!("{clipped}\n... (MCP text truncated)")
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
    client: tokio::sync::Mutex<Option<McpStdioClient>>,
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
        let tools = client.list_tools().await?;
        let tool_specs: Vec<ToolSpec> = tools
            .into_iter()
            .map(|tool| ToolSpec {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
                risk,
                output_budget: None,
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
        };
        Ok(Self {
            manifest,
            decl,
            request_timeout,
            max_frame_bytes,
            client: tokio::sync::Mutex::new(Some(client)),
        })
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
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            *guard = Some(
                McpClient::connect_stdio(&self.decl, self.request_timeout, self.max_frame_bytes)
                    .await?,
            );
        }
        let client = guard.as_mut().expect("client present after ensure");
        let result = client.call_tool(&call.name, call.arguments).await?;
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
        client.next_id = 1;
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

    #[test]
    fn text_clipping_is_bounded() {
        let long = "x".repeat(MAX_MCP_TOOL_TEXT_CHARS + 500);
        let clipped = clip_text(&long);
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
                    cancel: agent_contracts::CancellationToken::new(),
                },
            )
            .await
            .expect("invoke succeeds");
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

//! PLAT-04 共享适配器故障矩阵。
//!
//! 列是当前线协议，不是 Platform 信封（信封迁移在 PLAT-07）：
//! - process：`FramedProtocolSession` + 解析期 `JsonDecodeBudget`
//! - context：`serve_session` + 进程内 `AppendOnlyEngine`
//! - MCP：内存双工 `McpClient`
//!
//! 世代栅栏属于 Core/Platform，见 `agent-runtime` 的 turn/host 测试；
//! 本包生产依赖不得指向 `agent-core` / `agent-runtime`。
//! ProcessHost / 进程能力行在找不到 `mock_host` 时跳过（先
//! `cargo test -p agent-process` 才会把它编到 profile 目录）。

use std::time::Duration;

use agent_capability_process::{
    DEFAULT_MAX_SKIPPED_FRAMES_PER_REQUEST, MCP_PROTOCOL_VERSION, McpClient,
    ProcessCapabilityAdapter,
};
use agent_context_service::serve_session;
use agent_contracts::{
    CancellationToken, Capability, CapabilityInvocationContext, CapabilityKind,
    CapabilityLifecycle, CapabilityManifest, CapabilityStatus, CapabilityTransport, ToolCall,
    ToolRisk, ToolSpec,
};
use agent_platform_protocol::{JsonDecodeBudget, decode_value};
use agent_process::{FrameErrorKind, FramedProtocolSession, ProcessHost, ProcessHostConfig};
use context_baselines::AppendOnlyEngine;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

fn json_object_array(count: usize) -> String {
    let mut body = String::from("[");
    for index in 0..count {
        if index > 0 {
            body.push(',');
        }
        body.push_str("{}");
    }
    body.push(']');
    body
}

fn mock_host_program() -> Option<String> {
    let name = if cfg!(windows) {
        "mock_host.exe"
    } else {
        "mock_host"
    };
    let current = std::env::current_exe().ok()?;
    agent_process::probe_siblings(&current, name).map(|path| path.to_string_lossy().into_owned())
}

fn skip_without_mock_host() -> Option<String> {
    match mock_host_program() {
        Some(program) => Some(program),
        None => {
            eprintln!("skip ProcessHost matrix rows: mock_host is not built");
            None
        }
    }
}

async fn context_session(input: &[u8], max_frame_bytes: usize) -> Vec<u8> {
    let engine = AppendOnlyEngine::new();
    let mut reader = BufReader::new(input);
    let mut output = Vec::new();
    serve_session(&mut reader, &mut output, &engine, max_frame_bytes)
        .await
        .unwrap();
    output
}

fn context_error(output: &[u8]) -> String {
    let line = output.split(|byte| *byte == b'\n').next().unwrap_or(output);
    let value: Value = serde_json::from_slice(line).expect("one JSON error frame");
    assert_eq!(value["ok"], false);
    value["error"].as_str().expect("error string").to_string()
}

fn mcp_client(
    max_frame_bytes: u64,
) -> (
    McpClient<DuplexStream, DuplexStream>,
    DuplexStream,
    DuplexStream,
) {
    let (client_read, server_write) = tokio::io::duplex(512 * 1024);
    let (server_read, client_write) = tokio::io::duplex(64 * 1024);
    (
        McpClient::new(
            client_read,
            client_write,
            Duration::from_secs(5),
            max_frame_bytes,
        ),
        server_read,
        server_write,
    )
}

#[tokio::test]
async fn malformed_json_fails_closed() {
    let (mut peer, stream) = tokio::io::duplex(1024);
    let mut session = FramedProtocolSession::from_stream(stream, 1024).unwrap();
    peer.write_all(b"this is not json\n").await.unwrap();
    peer.flush().await.unwrap();
    let frame = session.recv().await.unwrap();
    let error = decode_value(&frame, &JsonDecodeBudget::for_frame_bytes(1024)).unwrap_err();
    assert!(
        error.to_string().contains("expected ident")
            || error.to_string().contains("expected value")
            || error.to_string().to_lowercase().contains("json"),
        "process decode must reject malformed JSON: {error}"
    );

    let error = context_error(&context_session(b"{not-json}\n", 1024).await);
    assert!(error.contains("bad request"), "context: {error}");

    let (mut client, _server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let _ = server_write.write_all(b"this is not json\n").await;
        let _ = server_write.flush().await;
        std::future::pending::<()>().await
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("parse") || error.to_string().contains("JSON"),
        "MCP: {error}"
    );
    assert!(client.is_poisoned());
}

#[tokio::test]
async fn truncated_and_partial_eof_fail_closed() {
    let (mut peer, stream) = tokio::io::duplex(1024);
    let mut session = FramedProtocolSession::from_stream(stream, 1024).unwrap();
    peer.write_all(b"{\"ok\":true").await.unwrap();
    peer.flush().await.unwrap();
    drop(peer);
    let error = session.recv().await.unwrap_err();
    assert_eq!(error.kind, FrameErrorKind::PartialEof);
    assert!(session.is_poisoned());

    let error = context_error(&context_session(b"{\"id\":1", 1024).await);
    assert!(error.contains("mid-frame"), "context: {error}");

    let (mut client, _server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let _ = server_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"result\":")
            .await;
        let _ = server_write.flush().await;
        drop(server_write);
        std::future::pending::<()>().await
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("mid-frame") || error.to_string().contains("poisoned"),
        "MCP: {error}"
    );
    assert!(client.is_poisoned());
}

#[tokio::test]
async fn oversize_frames_fail_closed() {
    let (mut peer, stream) = tokio::io::duplex(8192);
    let mut session = FramedProtocolSession::from_stream(stream, 32).unwrap();
    peer.write_all(format!("{}\n", "x".repeat(64)).as_bytes())
        .await
        .unwrap();
    peer.flush().await.unwrap();
    let error = session.recv().await.unwrap_err();
    assert!(matches!(error.kind, FrameErrorKind::Oversize { limit: 32 }));
    assert!(session.is_poisoned());

    let mut oversized = "{\"id\":1,\"version\":1,\"op\":\"ping\",\"pad\":\"".to_string();
    oversized.push_str(&"x".repeat(2048));
    oversized.push_str("\"}\n");
    // 帧帽必须大于错误响应，否则服务在越界后无法写出失败帧。
    let error = context_error(&context_session(oversized.as_bytes(), 256).await);
    assert!(
        error.contains("byte limit") || error.contains("bad request"),
        "context: {error}"
    );

    let (mut client, _server_read, mut server_write) = mcp_client(512);
    tokio::spawn(async move {
        let mut line = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"".to_vec();
        line.extend(std::iter::repeat_n(b'x', 2048));
        line.extend_from_slice(b"\"}]}}\n");
        let _ = server_write.write_all(&line).await;
        let _ = server_write.flush().await;
        std::future::pending::<()>().await
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(error.to_string().contains("byte limit"), "MCP: {error}");
    assert!(client.is_poisoned());
}

#[tokio::test]
async fn json_node_bomb_fails_closed_before_the_tree_inflates() {
    let bomb = json_object_array(70_000);
    let (mut peer, stream) = tokio::io::duplex(512 * 1024);
    let mut session = FramedProtocolSession::from_stream(stream, 512 * 1024).unwrap();
    peer.write_all(bomb.as_bytes()).await.unwrap();
    peer.write_all(b"\n").await.unwrap();
    peer.flush().await.unwrap();
    let frame = session.recv().await.expect("encoded bomb still fits");
    let error = decode_value(&frame, &JsonDecodeBudget::for_frame_bytes(512 * 1024)).unwrap_err();
    assert!(
        error.to_string().contains("json decode budget"),
        "process: {error}"
    );

    let mut context_bomb = String::from("{\"id\":1,\"version\":1,\"op\":\"ping\",\"pad\":");
    context_bomb.push_str(&json_object_array(200));
    context_bomb.push_str("}\n");
    assert!(context_bomb.len() < 1024);
    let error = context_error(&context_session(context_bomb.as_bytes(), 1024).await);
    assert!(error.contains("json decode budget"), "context: {error}");

    let (mut client, _server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let mut frame = Vec::from(&b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":"[..]);
        frame.extend_from_slice(json_object_array(70_000).as_bytes());
        frame.extend_from_slice(b"}\n");
        let _ = server_write.write_all(&frame).await;
        let _ = server_write.flush().await;
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("json decode budget"),
        "MCP: {error}"
    );
    assert!(client.is_poisoned());
}

#[tokio::test]
async fn version_and_schema_mismatch_fail_closed() {
    let error =
        context_error(&context_session(b"{\"id\":7,\"version\":99,\"op\":\"ping\"}\n", 1024).await);
    assert!(
        error.contains("version mismatch"),
        "context version: {error}"
    );
    let error = context_error(
        &context_session(b"{\"id\":8,\"version\":1,\"op\":\"not_a_real_op\"}\n", 1024).await,
    );
    assert!(error.contains("bad request"), "context schema: {error}");

    let (mut client, server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok().unwrap_or(0) == 0 {
            return;
        }
        let request: Value = serde_json::from_str(line.trim_end()).unwrap_or(json!({}));
        let id = request.get("id").cloned().unwrap_or(json!(1));
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "1999-01-01",
                "capabilities": {},
                "serverInfo": {"name": "old", "version": "0"}
            }
        });
        let mut frame = serde_json::to_string(&response).unwrap();
        frame.push('\n');
        let _ = server_write.write_all(frame.as_bytes()).await;
        let _ = server_write.flush().await;
    });
    let error = client.initialize().await.unwrap_err();
    assert!(
        error.to_string().contains("protocol version"),
        "MCP: {error}"
    );
    assert!(
        error.to_string().contains("1999-01-01")
            || error.to_string().contains(MCP_PROTOCOL_VERSION),
        "MCP version error should name the mismatch: {error}"
    );
    assert!(client.is_poisoned());
}

#[tokio::test]
async fn duplicate_or_stale_id_frames_fail_closed() {
    let (mut peer, stream) = tokio::io::duplex(4096);
    let mut session = FramedProtocolSession::from_stream(stream, 1024).unwrap();
    let coalesced = "{\"seq\":1}\n{\"seq\":2}\n";
    peer.write_all(coalesced.as_bytes()).await.unwrap();
    peer.flush().await.unwrap();
    let first = session.recv().await.unwrap();
    let second = session.recv().await.unwrap();
    assert_eq!(first, br#"{"seq":1}"#);
    assert_eq!(second, br#"{"seq":2}"#);

    let (mut client, server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let mut reader = BufReader::new(server_read);
        let mut line = String::new();
        let _ = reader.read_line(&mut line).await;
        let _ = server_write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"not-the-request\",\"result\":{}}\n")
            .await;
        let _ = server_write.flush().await;
        std::future::pending::<()>().await
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("id mismatch"),
        "MCP stale id: {error}"
    );
    assert!(client.is_poisoned());

    if let Some(program) = skip_without_mock_host() {
        let host = ProcessHost::connect(process_host_config(&program, Default::default()))
            .await
            .expect("mock handshake");
        let first = host.call(json!({ "op": "coalesced" })).await.unwrap();
        assert_eq!(first, json!("first"));
        let error = host.call(json!({ "op": "ping" })).await.unwrap_err();
        assert!(
            error.to_string().contains("id mismatch"),
            "ProcessHost stale id: {error}"
        );
        host.shutdown().await;
    }
}

#[tokio::test]
async fn notification_and_broker_floods_are_bounded() {
    let (mut client, _server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let flood = DEFAULT_MAX_SKIPPED_FRAMES_PER_REQUEST + 10;
        for _ in 0..flood {
            let _ = server_write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"server/notification\"}\n")
                .await;
        }
        let _ = server_write.flush().await;
        std::future::pending::<()>().await
    });
    let error = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        error.to_string().contains("notification flood"),
        "MCP: {error}"
    );
    assert!(client.is_poisoned());

    if let Some(program) = skip_without_mock_host() {
        let host = ProcessHost::connect(process_host_config(&program, Default::default()))
            .await
            .expect("mock handshake");
        // 无 broker 的 system 帧是断开/滥用：毒化而不是无限读。
        let error = host
            .call(json!({ "op": "system_abuse" }))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("system request") || error.to_string().contains("poisoned"),
            "process broker-less flood/abuse: {error}"
        );
        host.shutdown().await;
    }
}

#[tokio::test]
async fn cancel_late_poisons_so_a_late_frame_cannot_reconnect() {
    let (mut client, _server_read, _server_write) = mcp_client(1024 * 1024);
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        fire.cancel();
    });
    let error = client
        .call_tool_with_cancel("mock.echo", json!({}), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(error, agent_contracts::AgentError::Cancelled)
            || error.to_string().contains("cancelled"),
        "MCP cancel-late: {error}"
    );
    assert!(client.is_poisoned());
    let second = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "a cancelled MCP session must not admit a later call: {second}"
    );

    if let Some(program) = skip_without_mock_host() {
        let mut config = process_host_config(&program, Default::default());
        config.request_timeout = Duration::from_millis(400);
        let host = ProcessHost::connect(config).await.expect("mock handshake");
        let error = host.call(json!({ "op": "silent" })).await.unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "process cancel/timeout: {error}"
        );
        let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
        assert!(second.to_string().contains("poisoned"));
        host.shutdown().await;
    }
}

#[tokio::test]
async fn crash_or_poison_rejects_reuse_instead_of_silent_reconnect() {
    let (mut peer, stream) = tokio::io::duplex(1024);
    let mut session = FramedProtocolSession::from_stream(stream, 32).unwrap();
    peer.write_all(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n")
        .await
        .unwrap();
    let _ = session.recv().await;
    assert!(session.is_poisoned());
    let reuse = session.recv().await.unwrap_err();
    assert!(matches!(reuse.kind, FrameErrorKind::Poisoned { .. }));

    let output = context_session(
        b"{not-json}\n{\"id\":2,\"version\":1,\"op\":\"ping\"}\n",
        1024,
    )
    .await;
    // 会话在第一帧失败后关闭，不得再处理后续 ping。
    assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);

    let (mut client, _server_read, mut server_write) = mcp_client(1024 * 1024);
    tokio::spawn(async move {
        let _ = server_write.write_all(b"{not-json}\n").await;
        let _ = server_write.flush().await;
        std::future::pending::<()>().await
    });
    let _ = client.call_tool("mock.echo", json!({})).await;
    assert!(client.is_poisoned());
    let second = client.call_tool("mock.echo", json!({})).await.unwrap_err();
    assert!(second.to_string().contains("poisoned"));
}

#[tokio::test]
async fn effect_disconnect_quarantines_process_wire_effects() {
    let Some(program) = skip_without_mock_host() else {
        return;
    };
    let capability = ProcessCapabilityAdapter::with_config(
        CapabilityManifest {
            id: "process-demo".into(),
            version: "1.0.0".into(),
            name: "process demo".into(),
            summary: "matrix".into(),
            status: CapabilityStatus::Experimental,
            provides: vec![CapabilityKind::Tool],
            permissions: vec!["workspace:write".into()],
            requires: Vec::new(),
            tools: vec![ToolSpec {
                name: "process-demo.invoke".into(),
                description: "invoke".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::WorkspaceWrite,
                output_budget: None,
                roles: Vec::new(),
            }],
            lifecycle: CapabilityLifecycle::Lazy,
            transport: CapabilityTransport::Process {
                program: program.clone(),
            },
        },
        process_host_config(&program, Default::default()),
    );
    capability.start().await.unwrap();
    let error = capability
        .invoke(
            ToolCall {
                id: "matrix-effect".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({
                    "stage_write": {"path": "staged.txt", "content": "nope"}
                }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:write".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .expect_err("non-empty process wire effects stay quarantined");
    assert!(
        error
            .to_string()
            .contains("process wire effects are disabled"),
        "effect-disconnect: {error}"
    );
    capability.stop().await.unwrap();
}

#[test]
fn stale_generation_is_core_platform_not_an_isolated_adapter_fault() {
    // 世代栅栏在 Core/RuntimeActor（agent-runtime tests/turn.rs、host.rs）。
    // 隔离适配器看不到 Core generation，本矩阵不得引入 agent-runtime。
}

fn process_host_config(
    program: &str,
    offered_features: agent_platform_protocol::ActiveFeatures,
) -> ProcessHostConfig {
    ProcessHostConfig {
        program: program.to_string(),
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features,
        sandbox: Default::default(),
    }
}

//! A standalone JSON-lines mock child used by `agent-process`'s tests.
//!
//! Lives in the package's bin targets, so `cargo test -p agent-process`
//! builds it next to the test binaries (`target/<profile>/mock_host`) and
//! the tests spawn it with `--serve` to drive the framing and failure
//! scenarios against a real process. Without `--serve` it does nothing and
//! exits 0, so it can also be run directly.
//!
//! It refuses to serve unless `MOCK_MARKER=1` was injected by the parent —
//! that doubles as the test that `ProcessHostConfig.env` actually reaches
//! the child.

use std::time::Duration;

use agent_contracts::ToolOutput;
use agent_platform_protocol::{ActiveFeatures, FEATURE_LEGACY_INVOKE_OUTPUT};
use agent_process::PROTOCOL_VERSION;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

fn main() {
    if !std::env::args().any(|arg| arg == "--serve") {
        return; // plain `cargo test` run: nothing to do
    }
    let marker = std::env::var("MOCK_MARKER").unwrap_or_default();
    if marker != "1" {
        eprintln!("mock host requires MOCK_MARKER=1 (env injection test)");
        std::process::exit(1);
    }
    // The fixture is a sequential ping-pong server: a single-threaded
    // runtime keeps each spawned child to one thread, which matters when
    // audits spawn many children on a shared host.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("mock runtime");
    runtime.block_on(server_loop());
}

/// Mock child protocol. Speaks the same shape as the real service
/// (`{id, version, ok, value}`) plus two deliberate failure modes:
/// - `big` streams an oversized line without a newline (bounded-read test);
/// - `silent` never answers (request-deadline test).
///
/// When `MOCK_HEARTBEAT=<path>` is injected, a background task rewrites the
/// file with an incrementing counter every 50 ms — the *only* observable
/// liveness signal from outside the process, so a cancellation test can
/// prove the child tree was actually terminated (the counter stops).
/// One mid-invoke system request the mock issues, plus the `model_content`
/// prefixes the parent-side test asserts on the broker's answer. The ok and
/// refused prefixes differ per op family (`FS_READ`/`FS_REFUSED` for
/// `fs.read`, `NET_FETCH`/`NET_REFUSED` for `net.fetch`), so a test can
/// tell a refused read from a refused network request.
enum SystemProbe {
    FsRead { path: String },
    NetFetch,
    Unknown,
}

impl SystemProbe {
    fn frame(&self) -> Value {
        match self {
            SystemProbe::FsRead { path } => json!({ "system": "fs.read", "path": path }),
            SystemProbe::NetFetch => json!({ "system": "net.fetch", "url": "http://127.0.0.1/" }),
            SystemProbe::Unknown => json!({ "system": "no.such.op" }),
        }
    }

    fn ok_prefix(&self) -> &'static str {
        match self {
            SystemProbe::FsRead { .. } => "FS_READ",
            // Never observed today: network is deny-by-default, so the
            // broker always refuses; kept for symmetry with `fs.read`.
            SystemProbe::NetFetch => "NET_FETCH",
            SystemProbe::Unknown => "FS_READ",
        }
    }

    fn refused_prefix(&self) -> &'static str {
        match self {
            SystemProbe::FsRead { .. } => "FS_REFUSED",
            SystemProbe::NetFetch => "NET_REFUSED",
            SystemProbe::Unknown => "FS_REFUSED",
        }
    }
}

async fn server_loop() {
    // Bounded-stderr test hook: when the parent injects
    // `MOCK_STDERR_FLOOD_BYTES`, the mock writes that many bytes to stderr
    // at startup (plus a tail marker) — the host must drain it into a
    // bounded tail instead of buffering it all or inheriting it.
    if let Ok(flood) = std::env::var("MOCK_STDERR_FLOOD_BYTES")
        && let Ok(bytes) = flood.parse::<usize>()
    {
        use std::io::Write;
        let mut sink = std::io::stderr().lock();
        let chunk = "x".repeat(1024);
        let mut written = 0usize;
        while written < bytes {
            let take = chunk.len().min(bytes - written);
            let _ = sink.write_all(&chunk.as_bytes()[..take]);
            written += take;
        }
        let _ = writeln!(sink, "STDERR_TAIL_MARKER");
    }

    if let Ok(path) = std::env::var("MOCK_HEARTBEAT") {
        let path = std::path::PathBuf::from(path);
        // A dedicated thread, decoupled from the tokio runtime: the
        // heartbeat must tick regardless of how the async server loop is
        // scheduled, on any runtime configuration.
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            loop {
                let _ = std::fs::write(&path, n.to_string());
                n += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
        });
    }

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);

    while let Ok(Some(line)) = lines.next_line().await {
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let id = request.get("id").and_then(Value::as_u64).unwrap_or(0);
        match request.get("op").and_then(Value::as_str).unwrap_or("") {
            "ping" => reply_ping(&mut writer, id, &request).await,
            "cwd" => {
                // Echo the child's working directory (sandbox test: the
                // child must run in the dedicated cwd, not the parent's).
                let cwd = std::env::current_dir()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                reply(&mut writer, id, json!(cwd)).await;
            }
            "self_check" => {
                // The evaluation loop's test step: a generated capability
                // runs its own verification round inside the sandbox. The
                // check writes its artifact into the working directory and
                // reports the result — the sandbox test asserts the
                // artifact stays inside the sandboxed cwd, so the
                // verification itself is contained, not just env and cwd.
                let probe = std::env::current_dir()
                    .map(|cwd| cwd.join("self-check-result.json"))
                    .unwrap_or_default();
                let _ = std::fs::write(&probe, "{\"passed\":true}\n");
                reply(
                    &mut writer,
                    id,
                    json!({ "passed": true, "probe": probe.to_string_lossy() }),
                )
                .await;
            }
            "env" => {
                // Echo a variable (sandbox test: unlisted parent secrets
                // must not reach the child; explicit grants must).
                let value = std::env::var("SANDBOX_SECRET").unwrap_or_default();
                reply(&mut writer, id, json!(value)).await;
            }
            "invoke" => {
                // A canned `ToolOutput` so the process-capability adapter's
                // round trip is testable against a real process — unless the
                // call asks for `silent`, in which case the mock never
                // answers (cancellation test: the client must abort and kill
                // the tree instead of waiting out the request deadline).
                let silent = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("silent"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if silent {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                // `echo_env: "<name>"` replaces the canned content with that
                // environment variable's value — the sandbox scrub test's
                // window into the child's environment across the wire.
                let echo_env = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("echo_env"))
                    .and_then(Value::as_str);
                // `echo_permissions: true` surfaces the permissions the
                // parent granted this invocation — the wire proof that the
                // granted set reaches the experimental code intact.
                let echo_permissions = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("echo_permissions"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let permissions = request
                    .get("permissions")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                // Brokered-system test hooks: `ask_fs_read: "<path>"` makes
                // the mock issue a mid-invoke `{"system": "fs.read", ...}`
                // frame and wait for the host's answer; `ask_net_fetch:
                // true` issues a `{"system": "net.fetch", ...}` frame;
                // `ask_unknown_system: true` issues an undeclared op. The
                // outcome replaces the canned model content so the
                // parent-side test sees exactly what the broker allowed or
                // refused.
                let ask_fs_read = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("ask_fs_read"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let ask_net_fetch = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("ask_net_fetch"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let ask_unknown_system = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("ask_unknown_system"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let ask_fs_flood = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("ask_fs_flood"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let probe = if let Some(path) = ask_fs_read {
                    Some(SystemProbe::FsRead { path })
                } else if ask_net_fetch {
                    Some(SystemProbe::NetFetch)
                } else if ask_unknown_system {
                    Some(SystemProbe::Unknown)
                } else {
                    None
                };
                let model_content = match echo_env {
                    Some(name) => std::env::var(name).unwrap_or_default(),
                    None if echo_permissions => permissions.to_string(),
                    None => "process capability handled the call".into(),
                };
                let output = serde_json::to_value(ToolOutput {
                    call_id: "c1".into(),
                    tool_name: "process-demo.invoke".into(),
                    ok: true,
                    summary: "process ran".into(),
                    model_content,
                    artifact_ref: None,
                    metadata: json!({}),
                })
                .expect("ToolOutput serializes");
                // When a broker hook is set, run one system round trip now
                // and overwrite the model content with its outcome.
                // `ask_fs_flood` instead fires `MAX_SYSTEM_REQUESTS_PER_CALL
                // + 1` mid-invoke `fs.read` frames: the host must enforce
                // its per-call system-request cap and poison + kill the
                // tree. The final answer read fails (the tree is dead), so
                // the mock can only report the flood ran; the parent-side
                // test asserts the invoke itself failed with the poisoned
                // connection.
                let output = if ask_fs_flood {
                    let cap = agent_process::MAX_SYSTEM_REQUESTS_PER_CALL;
                    for _ in 0..(cap + 1) {
                        let _ = system_round_trip(
                            &mut writer,
                            &mut lines,
                            json!({ "system": "fs.read", "path": "x" }),
                        )
                        .await;
                    }
                    let mut output = output;
                    output["model_content"] = json!("FLOOD_DONE");
                    output
                } else if let Some(probe) = probe {
                    let answer = system_round_trip(&mut writer, &mut lines, probe.frame()).await;
                    let model_content = match (
                        answer.get("system_ok").and_then(Value::as_bool),
                        answer.get("value"),
                        answer.get("error").and_then(Value::as_str),
                    ) {
                        (Some(true), Some(value), _) => {
                            let content = value
                                .get("content_b64")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let bytes = base64::Engine::decode(
                                &base64::engine::general_purpose::STANDARD,
                                content,
                            )
                            .unwrap_or_default();
                            // The broker's read metadata rides along so the
                            // parent-side test can assert the bound: how many
                            // bytes the file had, and whether the served
                            // prefix was truncated to the broker's cap.
                            let byte_len =
                                value.get("byte_len").and_then(Value::as_u64).unwrap_or(0);
                            let truncated = value
                                .get("truncated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            format!(
                                "{}:{}\nFS_META:byte_len={byte_len},truncated={truncated}",
                                probe.ok_prefix(),
                                String::from_utf8_lossy(&bytes)
                            )
                        }
                        (_, _, Some(error)) => format!("{}:{error}", probe.refused_prefix()),
                        _ => format!("{}:malformed system answer", probe.refused_prefix()),
                    };
                    let mut output = output;
                    output["model_content"] = json!(model_content);
                    output
                } else {
                    output
                };
                // `stage_write: {"path": ..., "content": ...}` — the mock
                // declares a workspace-write *wire effect* instead of
                // mutating anything itself: the wire effect broker test
                // asserts the adapter stages it through the confined handle
                // and the runtime commits it behind the generation fence.
                let stage_write = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("stage_write"))
                    .and_then(Value::as_object);
                // 当前信封是 `{output, effects}`。纯 ToolOutput 只在调用方
                // 显式要 `legacy_plain` 时返回，供握手协商测试使用。
                let legacy_plain = request
                    .get("call")
                    .and_then(|call| call.get("arguments"))
                    .and_then(|args| args.get("legacy_plain"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let value = match stage_write {
                    Some(spec) => {
                        let path = spec
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or("staged.txt")
                            .to_string();
                        let content = spec
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .as_bytes()
                            .to_vec();
                        let content_b64 = base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            &content,
                        );
                        json!({
                            "output": output,
                            "effects": [{
                                "op": "workspace_write",
                                "path": path,
                                "content_b64": content_b64,
                            }],
                        })
                    }
                    None if legacy_plain => output,
                    None => json!({ "output": output, "effects": [] }),
                };
                reply(&mut writer, id, value).await;
            }
            "big" => {
                // Stream far more than any test's `max_frame_bytes` without
                // a newline: the client must reject while reading, not grow
                // a multi-megabyte buffer first and check it afterwards.
                let payload = "x".repeat(4 * 1024 * 1024);
                let _ = writer.write_all(payload.as_bytes()).await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "big_ok" => {
                // Answer with a large but frame-legal response (well below
                // the default frame cap): a per-call cumulative byte bound
                // must still trip when request + response exceed it.
                let response = json!({
                    "id": id,
                    "version": PROTOCOL_VERSION,
                    "ok": true,
                    "value": { "payload": "x".repeat(64 * 1024) },
                });
                let _ = writer
                    .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                    .await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
            }
            "coalesced" => {
                // Send the current response plus a guessed next response in
                // one write. A byte-stream codec must preserve both frames;
                // the session rejects the guessed request identity when the
                // next real call uses an unpredictable host-owned id.
                let first = json!({
                    "id": id,
                    "version": PROTOCOL_VERSION,
                    "ok": true,
                    "value": "first",
                });
                let second = json!({
                    "id": id + 1,
                    "version": PROTOCOL_VERSION,
                    "ok": true,
                    "value": "second",
                });
                let mut line = serde_json::to_string(&first).unwrap();
                line.push('\n');
                line.push_str(&serde_json::to_string(&second).unwrap());
                line.push('\n');
                let _ = writer.write_all(line.as_bytes()).await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "partial_eof" => {
                // Write half a frame (no terminating newline) and exit: the
                // client must fail closed on the partial frame instead of
                // accepting it, and the session must end.
                let _ = writer
                    .write_all(
                        format!(
                            "{{\"id\":{id},\"version\":{PROTOCOL_VERSION},\"ok\":true,\"value\":"
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = writer.flush().await;
                return;
            }
            "malformed" => {
                // A complete line that is not JSON: the client must treat
                // the unparseable frame as a framing violation and poison +
                // terminate, never keep the connection alive with unknown
                // framing state.
                let _ = writer.write_all(b"this is not json\n").await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "json_bomb" => {
                // 编码长度远小于默认 1 MiB 帧帽，但空对象数组会撑爆解码节点预算。
                let mut line =
                    format!("{{\"id\":{id},\"version\":{PROTOCOL_VERSION},\"ok\":true,\"value\":[");
                for index in 0..70_000 {
                    if index > 0 {
                        line.push(',');
                    }
                    line.push_str("{}");
                }
                line.push_str("]}\n");
                let _ = writer.write_all(line.as_bytes()).await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "invalid_utf8" => {
                // A newline-terminated frame that is not UTF-8. JSON is
                // UTF-8 by contract; the client must poison rather than
                // lossy-decode or reuse the session.
                let _ = writer.write_all(&[0xff, b'\n']).await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "bad_id" => {
                let response = json!({
                    "id": id + 1,
                    "version": PROTOCOL_VERSION,
                    "ok": true,
                    "value": "wrong request",
                });
                write_raw_response(&mut writer, &response).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "bad_version" => {
                let response = json!({
                    "id": id,
                    "version": PROTOCOL_VERSION + 1,
                    "ok": true,
                    "value": "wrong protocol",
                });
                write_raw_response(&mut writer, &response).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "bad_ok" => {
                let response = json!({
                    "id": id,
                    "version": PROTOCOL_VERSION,
                    "ok": "yes",
                    "value": "not a typed envelope",
                });
                write_raw_response(&mut writer, &response).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "domain_error" => {
                let response = json!({
                    "id": id,
                    "version": PROTOCOL_VERSION,
                    "ok": false,
                    "error": "expected domain failure",
                });
                write_raw_response(&mut writer, &response).await;
            }
            "silent" => {
                // Never answer: the client's request deadline must fire and
                // poison the connection (the request may have been written,
                // so a late response must never be mistaken for the next
                // request's answer).
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "ack_cancel" => {
                // Wait for the host's peer cancel frame, then ACK. Tests
                // that cancel-ACK is observed before kill-then-reap.
                loop {
                    let Ok(Some(line)) = lines.next_line().await else {
                        return;
                    };
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if msg.get("op").and_then(Value::as_str) == Some("cancel")
                        && msg.get("id").and_then(Value::as_u64) == Some(id)
                    {
                        let ack = json!({
                            "id": id,
                            "version": PROTOCOL_VERSION,
                            "cancelled": true,
                        });
                        write_raw_response(&mut writer, &ack).await;
                        break;
                    }
                }
            }
            "progress" => {
                // Several progress frames then the real answer. The host
                // must coalesce (drop intermediates) and admit only the
                // final value — progress is not a second inflight call.
                for seq in 0..4u32 {
                    let frame = json!({
                        "progress": true,
                        "id": id,
                        "seq": seq,
                        "note": format!("step {seq}"),
                    });
                    write_raw_response(&mut writer, &frame).await;
                }
                reply(&mut writer, id, json!("done")).await;
            }
            "progress_flood" => {
                let cap = agent_process::MAX_PROGRESS_FRAMES_PER_CALL;
                for seq in 0..=cap {
                    let frame = json!({
                        "progress": true,
                        "id": id,
                        "seq": seq,
                    });
                    write_raw_response(&mut writer, &frame).await;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "system_abuse" => {
                // Answer a request with a system frame instead of the normal
                // `{id, version, ok, value}` response: a host call without
                // a broker must refuse and poison (fail-closed), never
                // misparse the frame as a response.
                let _ = writer
                    .write_all(b"{\"system\":\"fs.read\",\"path\":\"x\"}\n")
                    .await;
                let _ = writer.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            "shutdown" => {
                reply(&mut writer, id, Value::Null).await;
                return;
            }
            _ => {}
        }
    }
}

/// One mid-invoke system round trip: write the child's `{"system": ...}`
/// frame, read the host's answer line, return it. The host answers each
/// system frame with a single line before the exchange continues.
async fn system_round_trip(
    writer: &mut BufWriter<tokio::io::Stdout>,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    request: Value,
) -> Value {
    let line = serde_json::to_string(&request).unwrap_or_default();
    let _ = writer.write_all(line.as_bytes()).await;
    let _ = writer.write_all(b"\n").await;
    let _ = writer.flush().await;
    match lines.next_line().await {
        Ok(Some(line)) => serde_json::from_str(&line).unwrap_or_default(),
        _ => json!({ "system_ok": false, "error": "host closed the connection" }),
    }
}

/// ping 始终声明本 mock 支持的遗留特性。宿主提供集为空时交集仍为空，
/// 子端不能单方面打开未提供的项。
async fn reply_ping(writer: &mut BufWriter<tokio::io::Stdout>, id: u64, request: &Value) {
    let mut response = json!({
        "id": id,
        "version": PROTOCOL_VERSION,
        "ok": true,
        "value": "pong",
        "epoch": 1u64,
    });
    let features = if std::env::var("MOCK_BAD_FEATURES").ok().as_deref() == Some("1") {
        json!(["z.v1", "a.v1"])
    } else {
        let supported =
            ActiveFeatures::new(vec![FEATURE_LEGACY_INVOKE_OUTPUT.into()]).expect("known feature");
        // 请求带了 features 时按提供 ∩ 支持回显；未带时仍声明支持集，
        // 让空提供集的宿主证明交集为空。
        let advertised = match ActiveFeatures::from_json_value(request.get("features")) {
            Ok(offered) if !offered.is_empty() => offered.intersect(&supported),
            _ => supported,
        };
        json!(advertised.as_slice())
    };
    response["features"] = features;
    write_raw_response(writer, &response).await;
}

async fn reply(writer: &mut BufWriter<tokio::io::Stdout>, id: u64, value: Value) {
    let response = json!({
        "id": id,
        "version": PROTOCOL_VERSION,
        "ok": true,
        "value": value,
    });
    write_raw_response(writer, &response).await;
}

async fn write_raw_response(writer: &mut BufWriter<tokio::io::Stdout>, response: &Value) {
    let _ = writer
        .write_all(serde_json::to_string(response).unwrap().as_bytes())
        .await;
    let _ = writer.write_all(b"\n").await;
    let _ = writer.flush().await;
}

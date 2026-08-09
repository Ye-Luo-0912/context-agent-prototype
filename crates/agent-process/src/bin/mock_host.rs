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
    let runtime = tokio::runtime::Runtime::new().expect("mock runtime");
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
async fn server_loop() {
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
            "ping" => reply(&mut writer, id, json!("pong")).await,
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
                reply(&mut writer, id, output).await;
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
            "silent" => {
                // Never answer: the client's request deadline must fire and
                // poison the connection (the request may have been written,
                // so a late response must never be mistaken for the next
                // request's answer).
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

async fn reply(writer: &mut BufWriter<tokio::io::Stdout>, id: u64, value: Value) {
    let response = json!({
        "id": id,
        "version": PROTOCOL_VERSION,
        "ok": true,
        "value": value,
    });
    let _ = writer
        .write_all(serde_json::to_string(&response).unwrap().as_bytes())
        .await;
    let _ = writer.write_all(b"\n").await;
    let _ = writer.flush().await;
}

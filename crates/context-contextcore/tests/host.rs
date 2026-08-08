//! End-to-end tests for the shared `ProcessHost`: bounded response frames,
//! per-request deadlines, and the poisoned-connection policy.
//!
//! The child is the `mock_host` test target (see `mock_host.rs`) — a real
//! process speaking the same JSON-lines shape as the context service, with
//! two deliberate failure modes (`big` streams an oversized frame, `silent`
//! never answers). The mock also requires `MOCK_MARKER=1`, so the handshake
//! doubles as proof that `ProcessHostConfig.env` reaches the child.

mod common;

use std::time::Duration;

use context_contextcore::{ProcessHost, ProcessHostConfig};
use serde_json::json;

/// Spawn the `mock_host` test target with `--serve` and the env marker.
/// Note: run `cargo test -p context-contextcore` (not `--test host`) so the
/// mock target is built alongside this one.
async fn spawn_mock(tune: impl FnOnce(&mut ProcessHostConfig)) -> ProcessHost {
    let program = common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host binary; run `cargo test -p context-contextcore`")
        });
    let mut config = ProcessHostConfig {
        program,
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
    };
    tune(&mut config);
    ProcessHost::connect(config)
        .await
        .expect("mock handshake within the startup deadline")
}

#[tokio::test]
async fn normal_exchange_and_graceful_shutdown() {
    let host = spawn_mock(|_| {}).await;
    let value = host.call(json!({ "op": "ping" })).await.unwrap();
    assert_eq!(value, json!("pong"));
    host.shutdown().await;
}

#[tokio::test]
async fn oversized_response_frame_is_rejected_while_reading_and_poisons() {
    let host = spawn_mock(|config| config.max_frame_bytes = 128).await;
    let error = host.call(json!({ "op": "big" })).await.unwrap_err();
    assert!(
        error.to_string().contains("byte limit"),
        "expected a byte-limit error, got: {error}"
    );

    // The connection is poisoned: a half-read exchange must never be reused
    // for a later request, so every further call fails fast.
    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "expected a poisoned-connection error, got: {second}"
    );
}

#[tokio::test]
async fn silent_request_times_out_and_poisons_the_connection() {
    let host = spawn_mock(|config| config.request_timeout = Duration::from_millis(400)).await;
    let error = host.call(json!({ "op": "silent" })).await.unwrap_err();
    assert!(
        error.to_string().contains("timed out"),
        "expected a timeout error, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "expected a poisoned-connection error, got: {second}"
    );
}

//! End-to-end tests for the shared `ProcessHost`: bounded response frames,
//! per-request deadlines, and the poisoned-connection policy.
//!
//! The child is the `mock_host` bin (see `src/bin/mock_host.rs`) — a real
//! process speaking the same JSON-lines shape as the context service, with
//! two deliberate failure modes (`big` streams an oversized frame, `silent`
//! never answers). The mock also requires `MOCK_MARKER=1`, so the handshake
//! doubles as proof that `ProcessHostConfig.env` reaches the child.

mod common;

use std::time::Duration;

use agent_process::{ProcessHost, ProcessHostConfig, ProcessSandbox};
use serde_json::json;

/// Spawn the `mock_host` bin with `--serve` and the env marker.
/// Note: run `cargo test -p agent-process` (not `--test host`) so the mock
/// bin is built alongside this one.
async fn spawn_mock(tune: impl FnOnce(&mut ProcessHostConfig)) -> ProcessHost {
    let program = common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host bin; run `cargo test -p agent-process`")
        });
    let mut config = ProcessHostConfig {
        program,
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        sandbox: Default::default(),
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

#[tokio::test]
async fn sandbox_drops_unlisted_secrets_and_forces_the_dedicated_cwd() {
    // A parent secret the whitelist does not name: it must never reach the
    // child. Explicit grants (via `env`) still arrive.
    unsafe { std::env::set_var("SANDBOX_SECRET", "parent-secret") };
    let host = spawn_mock(|config| {
        config.sandbox = ProcessSandbox {
            env_whitelist: Some(vec!["PATH".into()]),
            cwd: Some(std::env::temp_dir().join(format!("sandbox-cwd-{}", std::process::id()))),
            cpu_time_limit_secs: 0,
            process_limit: 0,
        };
    })
    .await;
    let cwd = host.call(json!({ "op": "cwd" })).await.unwrap();
    assert!(
        cwd.as_str().unwrap_or_default().contains("sandbox-cwd"),
        "the child must run in its dedicated cwd, got: {cwd}"
    );
    let secret = host.call(json!({ "op": "env" })).await.unwrap();
    assert_eq!(
        secret,
        json!(""),
        "an unlisted parent secret must not be inherited"
    );
    host.shutdown().await;

    // An explicit grant crosses the boundary even under a strict whitelist.
    let host = spawn_mock(|config| {
        config.sandbox = ProcessSandbox {
            env_whitelist: Some(vec!["PATH".into()]),
            cwd: None,
            cpu_time_limit_secs: 0,
            process_limit: 0,
        };
        config
            .env
            .push(("SANDBOX_SECRET".into(), "granted-value".into()));
    })
    .await;
    let secret = host.call(json!({ "op": "env" })).await.unwrap();
    assert_eq!(
        secret,
        json!("granted-value"),
        "explicitly granted variables must reach the child"
    );
    host.shutdown().await;
}

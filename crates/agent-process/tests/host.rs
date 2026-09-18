//! End-to-end tests for the shared `ProcessHost`: bounded frames in both
//! directions, per-request deadlines, and the poisoned-connection policy.
//!
//! The child is the `mock_host` bin (see `src/bin/mock_host.rs`) — a real
//! process speaking the same JSON-lines shape as the context service, with
//! deliberate failure modes (`big` streams an oversized frame, `silent`
//! never answers, while `partial_eof`/`malformed` violate framing).
//! The mock also requires `MOCK_MARKER=1`, so the handshake doubles as
//! proof that `ProcessHostConfig.env` reaches the child.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::CancellationToken;
use agent_platform_protocol::{ActiveFeatures, FEATURE_LEGACY_INVOKE_OUTPUT};
use agent_process::{ConnectionHealth, ProcessHost, ProcessHostConfig, ProcessSandbox};
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
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
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
    let status = host.status();
    assert_eq!(status.health, ConnectionHealth::Ready);
    assert_eq!(status.epoch.host, 1);
    assert_eq!(status.epoch.peer, 1, "mock ping advertises epoch 1");
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
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
    assert_eq!(host.status().epoch.host, 1);
}

#[tokio::test]
async fn peer_cancel_ack_quarantines_and_does_not_admit_a_value() {
    let host = spawn_mock(|_| {}).await;
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        fire.cancel();
    });
    let error = host
        .call_with_cancel(json!({ "op": "ack_cancel" }), &cancel)
        .await
        .unwrap_err();
    assert!(
        matches!(error, agent_contracts::AgentError::Cancelled),
        "cancel-ACK must surface as Cancelled, got {error}"
    );
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "cancel-ACK still kill-then-reaps: {second}"
    );
}

#[tokio::test]
async fn acknowledged_cancel_still_kills_the_child() {
    // The mock writes a heartbeat file from its own thread as long as the
    // process lives. An ACKed cancel must still kill-then-reap: the ACK is
    // optional evidence, never permission to keep the child running.
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("alive.ticks");
    let host = spawn_mock(|config| {
        config.env.push((
            "MOCK_HEARTBEAT".into(),
            heartbeat.to_string_lossy().into_owned(),
        ));
    })
    .await;
    for _ in 0..100 {
        if std::fs::read(&heartbeat).is_ok_and(|bytes| !bytes.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        std::fs::read(&heartbeat).is_ok_and(|bytes| !bytes.is_empty()),
        "the mock heartbeat must be running before the cancel"
    );

    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        fire.cancel();
    });
    let error = host
        .call_with_cancel(json!({ "op": "ack_cancel" }), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, agent_contracts::AgentError::Cancelled));

    // The heartbeat thread dies with the child: the file stops updating.
    let before = std::fs::read(&heartbeat).unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = std::fs::read(&heartbeat).unwrap_or_default();
    assert_eq!(
        before, after,
        "an acknowledged cancel must still kill the child, not trust the ACK"
    );
}

#[tokio::test]
async fn cancel_aborts_broker_work_promptly() {
    // A broker that stalls answering a brokered fs.read must not defuse
    // cancel: the host settles the call kill-then-reap well inside the
    // request deadline instead of waiting for the broker to finish.
    struct StallingBroker;
    #[async_trait::async_trait]
    impl agent_process::SystemBroker for StallingBroker {
        async fn handle(
            &self,
            _request: serde_json::Value,
        ) -> agent_contracts::AgentResult<serde_json::Value> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(serde_json::json!("late"))
        }
    }

    let host = spawn_mock(|config| config.request_timeout = Duration::from_secs(10)).await;
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        fire.cancel();
    });
    let started = std::time::Instant::now();
    let error = host
        .call_with_cancel_and_broker(
            json!({
                "op": "invoke",
                "call": {
                    "id": "c-broker",
                    "name": "process-demo.invoke",
                    "arguments": { "ask_fs_read": "notes.txt" }
                }
            }),
            &cancel,
            &StallingBroker,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, agent_contracts::AgentError::Cancelled),
        "a cancel during broker work must surface as Cancelled: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel must abort broker work well inside the request deadline"
    );
    assert_eq!(
        host.status().health,
        ConnectionHealth::Quarantined,
        "the settled connection must not be reused"
    );
}

#[tokio::test]
async fn cancel_without_peer_ack_still_kills_after_the_bound() {
    // Keep this tight wall-clock assertion out of the three concurrent
    // blocked-write fixtures below. The production ACK/kill bound is already
    // exercised by the isolated test; this slot prevents the test harness
    // from turning shared Windows CPU pressure into a false semantic failure.
    let _slot = BLOCKED_WRITE_SLOT.lock().await;
    let host = spawn_mock(|config| config.request_timeout = Duration::from_secs(5)).await;
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        fire.cancel();
    });
    let started = std::time::Instant::now();
    let error = host
        .call_with_cancel(json!({ "op": "silent" }), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, agent_contracts::AgentError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a silent peer must not stall cancel past the ACK bound"
    );
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
}

#[tokio::test]
async fn cancel_before_write_leaves_the_connection_usable() {
    let host = spawn_mock(|_| {}).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = host
        .call_with_cancel(json!({ "op": "ping" }), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, agent_contracts::AgentError::Cancelled));
    assert_eq!(host.status().health, ConnectionHealth::Ready);
    let value = host.call(json!({ "op": "ping" })).await.unwrap();
    assert_eq!(value, json!("pong"));
    host.shutdown().await;
}

/// The blocked-write tests each hold a real wedged child for their whole
/// body; serializing them keeps the suite's self-inflicted process load
/// flat so the tight pre-existing cancel-budget assertions that share the
/// machine stay reliable.
static BLOCKED_WRITE_SLOT: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Wait until the mock child confirms it consumed `expected` bytes of the
/// frame the host is writing — parent-side proof that the write had
/// actually started before the test fires the cancel token.
async fn wait_for_partial_write_evidence(path: &std::path::Path, expected: usize) {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(consumed) = text.trim().parse::<usize>()
            && consumed >= expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the child never confirmed consuming {expected} bytes of the frame");
}

#[tokio::test]
async fn cancel_stops_a_blocked_request_write_within_the_cancel_budget() {
    let _slot = BLOCKED_WRITE_SLOT.lock().await;
    // The child answers the handshake and then stops reading stdin. The
    // request (256 KiB, inside the frame contract but far above any OS pipe
    // buffer) wedges the host's `write_all` on pipe backpressure — the mock
    // confirms it consumed the first 64 KiB, so the write had really
    // started. The cancel token must stop the call in a cancel-sized
    // budget, not at the 30s request deadline; the half-written frame
    // poisons the connection and the child is terminated.
    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("read.snapshot");
    let heartbeat = dir.path().join("alive.ticks");
    let host = Arc::new(
        spawn_mock(|config| {
            config.request_timeout = Duration::from_secs(30);
            config.max_frame_bytes = 1024 * 1024;
            config.max_call_bytes = 4 * 1024 * 1024;
            config.env.push((
                "MOCK_HEARTBEAT".into(),
                heartbeat.to_string_lossy().into_owned(),
            ));
            config
                .env
                .push(("MOCK_STOP_READING_AFTER_BYTES".into(), "65536".into()));
            config.env.push((
                "MOCK_READ_SNAPSHOT".into(),
                snapshot.to_string_lossy().into_owned(),
            ));
        })
        .await,
    );

    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    let call_host = Arc::clone(&host);
    let call_task = tokio::spawn(async move {
        call_host
            .call_with_cancel(
                json!({ "op": "ping", "payload": "x".repeat(256 * 1024) }),
                &cancel,
            )
            .await
    });

    wait_for_partial_write_evidence(&snapshot, 65536).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !call_task.is_finished(),
        "the frame write must still be wedged: the child stopped reading at 64 KiB of a 256 KiB frame"
    );

    fire.cancel();
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(10), call_task)
        .await
        .expect("the call must end after cancel, not at the request deadline")
        .unwrap()
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(error, agent_contracts::AgentError::Cancelled),
        "a cancelled half-written request must surface as Cancelled, got: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "cancel must stop a wedged write inside the cancel budget, took {elapsed:?}"
    );
    assert_eq!(
        host.status().health,
        ConnectionHealth::Quarantined,
        "a connection with a half-written frame must not be reusable"
    );
    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "the half-frame connection must be fenced off: {second}"
    );
    // The child tree was terminated: its heartbeat stops ticking.
    let before = std::fs::read(&heartbeat).unwrap_or_default();
    assert!(
        !before.is_empty(),
        "the child must have been alive to cancel"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = std::fs::read(&heartbeat).unwrap_or_default();
    assert_eq!(
        before, after,
        "the child must be terminated after the cancel"
    );
}

#[tokio::test]
async fn cancel_stops_a_blocked_broker_answer_write_within_the_cancel_budget() {
    let _slot = BLOCKED_WRITE_SLOT.lock().await;
    // Same wedge, one stage later: the child emits a mid-invoke system
    // request and stops reading; the broker's 256 KiB answer wedges the
    // host's answer write. Cancel must still stop the call promptly.
    struct BigBroker;
    #[async_trait::async_trait]
    impl agent_process::SystemBroker for BigBroker {
        async fn handle(
            &self,
            _request: serde_json::Value,
        ) -> agent_contracts::AgentResult<serde_json::Value> {
            Ok(json!({ "content_b64": "x".repeat(256 * 1024) }))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("read.snapshot");
    let host = Arc::new(
        spawn_mock(|config| {
            config.request_timeout = Duration::from_secs(30);
            config.max_frame_bytes = 1024 * 1024;
            config.max_call_bytes = 4 * 1024 * 1024;
            config.max_system_answer_bytes = 1024 * 1024;
            config
                .env
                .push(("MOCK_STOP_READING_AFTER_BYTES".into(), "65536".into()));
            config
                .env
                .push(("MOCK_EMIT_SYSTEM_FRAME".into(), "1".into()));
            config.env.push((
                "MOCK_READ_SNAPSHOT".into(),
                snapshot.to_string_lossy().into_owned(),
            ));
        })
        .await,
    );

    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    let call_host = Arc::clone(&host);
    let call_task = tokio::spawn(async move {
        call_host
            .call_with_cancel_and_broker(json!({ "op": "ping" }), &cancel, &BigBroker)
            .await
    });

    wait_for_partial_write_evidence(&snapshot, 65536).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !call_task.is_finished(),
        "the broker answer write must still be wedged"
    );

    fire.cancel();
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(10), call_task)
        .await
        .expect("the call must end after cancel, not at the request deadline")
        .unwrap()
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(error, agent_contracts::AgentError::Cancelled),
        "a cancelled half-written broker answer must surface as Cancelled, got: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "cancel must stop a wedged broker answer inside the cancel budget, took {elapsed:?}"
    );
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "the half-frame connection must be fenced off: {second}"
    );
}

#[tokio::test]
async fn request_deadline_stops_a_blocked_write_and_poisons() {
    let _slot = BLOCKED_WRITE_SLOT.lock().await;
    // Without a cancel token the request deadline is the bound: a write
    // wedged on pipe backpressure must end at the deadline with the
    // connection poisoned (the child consumed only the first 64 KiB of the
    // 256 KiB frame, so the frame was half-written).
    let dir = tempfile::tempdir().unwrap();
    let snapshot = dir.path().join("read.snapshot");
    let host = spawn_mock(|config| {
        config.request_timeout = Duration::from_secs(2);
        config.max_frame_bytes = 1024 * 1024;
        config.max_call_bytes = 4 * 1024 * 1024;
        config
            .env
            .push(("MOCK_STOP_READING_AFTER_BYTES".into(), "65536".into()));
        config.env.push((
            "MOCK_READ_SNAPSHOT".into(),
            snapshot.to_string_lossy().into_owned(),
        ));
    })
    .await;

    let call = host.call(json!({ "op": "ping", "payload": "x".repeat(256 * 1024) }));
    let started = std::time::Instant::now();
    // Drive the call while waiting for the partial-write evidence.
    let error = tokio::select! {
        result = call => result.unwrap_err(),
        _ = tokio::time::sleep(Duration::from_secs(10)) => {
            panic!("the wedged write must end at the request deadline");
        }
    };
    wait_for_partial_write_evidence(&snapshot, 65536).await;
    let elapsed = started.elapsed();
    assert!(
        error.to_string().contains("timed out"),
        "a deadline-stopped wedged write must report the timeout, got: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the deadline must bound the wedged write, took {elapsed:?}"
    );
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(second.to_string().contains("poisoned"));
}

#[tokio::test]
async fn cancel_before_any_write_keeps_the_brokered_connection_usable() {
    // Cancel fired before the exchange wrote anything: the call ends as
    // Cancelled with no bytes on the pipe, so the same connection must
    // still complete a normal exchange afterwards.
    struct UnusedBroker;
    #[async_trait::async_trait]
    impl agent_process::SystemBroker for UnusedBroker {
        async fn handle(
            &self,
            _request: serde_json::Value,
        ) -> agent_contracts::AgentResult<serde_json::Value> {
            panic!("no system request may be brokered after a pre-write cancel");
        }
    }

    let host = spawn_mock(|_| {}).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = host
        .call_with_cancel_and_broker(json!({ "op": "ping" }), &cancel, &UnusedBroker)
        .await
        .unwrap_err();
    assert!(matches!(error, agent_contracts::AgentError::Cancelled));
    assert_eq!(host.status().health, ConnectionHealth::Ready);
    let value = host.call(json!({ "op": "ping" })).await.unwrap();
    assert_eq!(value, json!("pong"));
    host.shutdown().await;
}

#[tokio::test]
async fn progress_frames_are_coalesced_and_the_final_value_is_admitted() {
    let host = spawn_mock(|_| {}).await;
    let value = host.call(json!({ "op": "progress" })).await.unwrap();
    assert_eq!(value, json!("done"));
    host.shutdown().await;
}

#[tokio::test]
async fn progress_flood_poisons_the_connection() {
    let host = spawn_mock(|_| {}).await;
    let error = host
        .call(json!({ "op": "progress_flood" }))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("too many progress frames"),
        "progress flood must trip the per-call cap: {error}"
    );
    assert_eq!(host.status().health, ConnectionHealth::Quarantined);
}

#[tokio::test]
async fn outbound_oversize_request_is_rejected_before_any_byte_is_written() {
    let host = spawn_mock(|config| config.max_frame_bytes = 128).await;
    // The request alone exceeds the cap. Nothing may be written: the child
    // never sees a truncated frame, and the connection stays usable.
    let error = host
        .call(json!({ "op": "ping", "payload": "x".repeat(1024) }))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("above the 128 byte bound"),
        "expected an outbound cap error, got: {error}"
    );

    let value = host.call(json!({ "op": "ping" })).await.unwrap();
    assert_eq!(
        value,
        json!("pong"),
        "a rejected request must not poison the connection (nothing was written)"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn caller_cannot_override_host_owned_request_identity_or_version() {
    let host = spawn_mock(|_| {}).await;
    let value = host
        .call(json!({ "op": "ping", "id": 0, "version": 999 }))
        .await
        .unwrap();
    assert_eq!(value, json!("pong"));
    host.shutdown().await;
}

#[tokio::test]
async fn cumulative_call_bytes_are_bounded_and_poison() {
    // The response is frame-legal but the request + response together blow
    // the tiny per-call budget: the exchange must fail closed.
    let host = spawn_mock(|config| {
        config.max_frame_bytes = 1024 * 1024;
        config.max_call_bytes = 1024;
    })
    .await;
    let error = host.call(json!({ "op": "big_ok" })).await.unwrap_err();
    assert!(
        error.to_string().contains("per-call bound"),
        "expected a cumulative byte-limit error, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "the connection must be poisoned after the cumulative bound trips: {second}"
    );
}

#[tokio::test]
async fn presend_with_wrong_unpredictable_id_poisons_the_session() {
    let host = spawn_mock(|_| {}).await;
    let first = host.call(json!({ "op": "coalesced" })).await.unwrap();
    assert_eq!(first, json!("first"));
    let error = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        error.to_string().contains("id mismatch"),
        "the pre-sent frame must not match the unpredictable next request: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "the connection must be poisoned after a pre-sent response: {second}"
    );
}

#[tokio::test]
async fn partial_eof_frame_poisons_and_terminates_the_session() {
    let host = spawn_mock(|_| {}).await;
    let error = host.call(json!({ "op": "partial_eof" })).await.unwrap_err();
    assert!(
        error.to_string().contains("mid-frame"),
        "a partial EOF frame must fail closed, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "the connection must be poisoned after a partial frame: {second}"
    );
}

#[tokio::test]
async fn malformed_frame_poisons_and_terminates_the_session() {
    let host = spawn_mock(|_| {}).await;
    let error = host.call(json!({ "op": "malformed" })).await.unwrap_err();
    assert!(
        error.to_string().contains("parse response"),
        "expected a parse violation, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "an unparseable frame must poison the connection: {second}"
    );
}

#[tokio::test]
async fn decoded_json_node_budget_poisons_the_session() {
    let host = spawn_mock(|_| {}).await;
    let error = host.call(json!({ "op": "json_bomb" })).await.unwrap_err();
    assert!(
        error.to_string().contains("json decode budget"),
        "a frame-legal empty-object array must fail the decoded node budget, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(
        second.to_string().contains("poisoned"),
        "a JSON DOM bomb must poison the connection: {second}"
    );
}

#[tokio::test]
async fn invalid_utf8_poisons_and_terminates_the_session() {
    let host = spawn_mock(|_| {}).await;
    let error = host
        .call(json!({ "op": "invalid_utf8" }))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("not UTF-8"),
        "expected a UTF-8 framing violation, got: {error}"
    );

    let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
    assert!(second.to_string().contains("poisoned"));
}

#[tokio::test]
async fn invalid_response_identity_version_and_status_poison_the_session() {
    for (op, expected) in [
        ("bad_id", "id mismatch"),
        ("bad_version", "version mismatch"),
        ("bad_ok", "must be a boolean"),
    ] {
        let host = spawn_mock(|_| {}).await;
        let error = host.call(json!({ "op": op })).await.unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "{op} should report {expected:?}, got: {error}"
        );
        let second = host.call(json!({ "op": "ping" })).await.unwrap_err();
        assert!(
            second.to_string().contains("poisoned"),
            "{op} must poison the session: {second}"
        );
    }
}

#[tokio::test]
async fn typed_domain_error_does_not_poison_a_well_framed_session() {
    let host = spawn_mock(|_| {}).await;
    let error = host
        .call(json!({ "op": "domain_error" }))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected domain failure"));

    let value = host.call(json!({ "op": "ping" })).await.unwrap();
    assert_eq!(value, json!("pong"));
    host.shutdown().await;
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
            ..ProcessSandbox::default()
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
            ..ProcessSandbox::default()
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

#[tokio::test]
async fn handshake_keeps_legacy_invoke_output_off_when_not_offered() {
    // mock 始终声明支持遗留特性；提供集为空时交集必须仍为空。
    let host = spawn_mock(|_| {}).await;
    assert!(
        !host.allows_feature(FEATURE_LEGACY_INVOKE_OUTPUT),
        "a child cannot enable an unoffered feature"
    );
    assert!(host.negotiated_features().is_empty());
    host.shutdown().await;
}

#[tokio::test]
async fn handshake_enables_legacy_invoke_output_when_both_sides_offer_it() {
    let host = spawn_mock(|config| {
        config.offered_features =
            ActiveFeatures::new(vec![FEATURE_LEGACY_INVOKE_OUTPUT.into()]).expect("known feature");
    })
    .await;
    assert!(host.allows_feature(FEATURE_LEGACY_INVOKE_OUTPUT));
    host.shutdown().await;
}

#[tokio::test]
async fn handshake_rejects_unsorted_child_features() {
    let program = common::locate_mock_host()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            panic!("cannot locate the mock_host bin; run `cargo test -p agent-process`")
        });
    let error = match ProcessHost::connect(ProcessHostConfig {
        program,
        args: vec!["--serve".into()],
        env: vec![
            ("MOCK_MARKER".into(), "1".into()),
            ("MOCK_BAD_FEATURES".into(), "1".into()),
        ],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox: Default::default(),
    })
    .await
    {
        Ok(_) => panic!("unsorted child features must fail the handshake"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("handshake features"),
        "invalid advertised features must poison connect: {error}"
    );
}

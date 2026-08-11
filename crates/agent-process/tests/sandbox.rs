//! Process sandbox tests: the child's execution boundary (env whitelist,
//! dedicated cwd) driven against the real `mock_host` child.
//!
//! The sandbox is the enforcement point for "experimental code cannot
//! exceed the permissions granted to it": unlisted parent variables
//! (secrets) never cross, whitelisted variables and explicit grants do,
//! and the child never runs in the parent's cwd.

use std::sync::Mutex;
use std::time::Duration;

use agent_process::{ProcessHost, ProcessHostConfig, ProcessSandbox};
use serde_json::Value;

/// Locate the `mock_host` bin of this package (built next to the test
/// binaries in `target/<profile>/`).
fn locate_mock_host() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "mock_host.exe"
    } else {
        "mock_host"
    };
    let current = std::env::current_exe().ok()?;
    agent_process::probe_siblings(&current, name)
}

/// The tests that plant a parent-environment secret mutate the shared
/// process environment, and `cargo test` runs tests in parallel in one
/// process. The lock serializes "mutate env -> spawn child" so one test's
/// secret can never leak into another's spawn; after spawn the child's
/// environment is snapshotted and the parent can be released.
static ENV_LOCK: Mutex<()> = Mutex::new(());

async fn connect_with(sandbox: ProcessSandbox) -> ProcessHost {
    let program = locate_mock_host().expect("mock_host built");
    ProcessHost::connect(ProcessHostConfig {
        program: program.to_string_lossy().into_owned(),
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        sandbox,
    })
    .await
    .expect("mock host connects")
}

/// The mock's `env` op echoes its `SANDBOX_SECRET`.
async fn echo_secret(host: &ProcessHost) -> String {
    let value: Value = host
        .call(serde_json::json!({ "op": "env" }))
        .await
        .expect("env op answers");
    value.as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn unlisted_parent_env_is_scrubbed_from_the_child() {
    // A "secret" in the parent environment that the whitelist does not
    // name: it must never reach the child. Only the whitelisted variable
    // (plus the explicit MOCK_MARKER grant) crosses the boundary.
    //
    // SAFETY: the mutation happens under ENV_LOCK and the lock is released
    // before any await. The parallel tests plant the same value and none
    // removes it, so by spawn time the parent environment deterministically
    // carries the secret.
    {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("SANDBOX_SECRET", "parent-secret-value");
        }
    }
    let host = connect_with(ProcessSandbox {
        env_whitelist: Some(vec!["PATH".into()]),
        ..ProcessSandbox::default()
    })
    .await;
    assert_eq!(
        echo_secret(&host).await,
        "",
        "an unlisted parent variable must not cross the sandbox"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn whitelisted_parent_env_survives_the_scrub() {
    {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("SANDBOX_SECRET", "parent-secret-value");
        }
    }
    let host = connect_with(ProcessSandbox {
        env_whitelist: Some(vec!["PATH".into(), "SANDBOX_SECRET".into()]),
        ..ProcessSandbox::default()
    })
    .await;
    assert_eq!(
        echo_secret(&host).await,
        "parent-secret-value",
        "a whitelisted variable is inherited from the parent"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn explicit_env_grants_land_after_the_whitelist() {
    // The variable is NOT whitelisted, but the config grants it
    // explicitly: the grant must still reach the child. This needs no
    // parent-environment mutation, so it needs no lock.
    let program = locate_mock_host().expect("mock_host built");
    let granted = ProcessHost::connect(ProcessHostConfig {
        program: program.to_string_lossy().into_owned(),
        args: vec!["--serve".into()],
        env: vec![
            ("MOCK_MARKER".into(), "1".into()),
            ("SANDBOX_SECRET".into(), "explicit-grant".into()),
        ],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        sandbox: ProcessSandbox {
            env_whitelist: Some(vec!["PATH".into()]),
            ..ProcessSandbox::default()
        },
    })
    .await
    .expect("granted host connects");
    assert_eq!(
        echo_secret(&granted).await,
        "explicit-grant",
        "an explicit env grant lands even when the variable is not whitelisted"
    );
    granted.shutdown().await;
}

#[tokio::test]
async fn sandbox_cwd_is_created_and_isolates_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("child-workdir");
    let parent_cwd = std::env::current_dir().unwrap();
    let host = connect_with(ProcessSandbox {
        env_whitelist: Some(vec!["PATH".into()]),
        cwd: Some(cwd.clone()),
        ..ProcessSandbox::default()
    })
    .await;
    assert!(
        cwd.exists(),
        "connect must create the sandbox working directory"
    );
    let child_cwd: String = host
        .call(serde_json::json!({ "op": "cwd" }))
        .await
        .expect("cwd op answers")
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        child_cwd,
        cwd.to_string_lossy(),
        "the child must run in its dedicated working directory"
    );
    assert_ne!(
        child_cwd,
        parent_cwd.to_string_lossy(),
        "the child must never run in the parent's cwd"
    );
    host.shutdown().await;
}

/// The V2 evaluation-loop link: a generated capability's self-check runs
/// inside the strict sandbox, and the check's own artifacts cannot escape
/// it. The mock's `self_check` op writes a verification artifact into its
/// working directory and reports the result; the parent must see the pass
/// but no trace of the artifact outside the sandboxed cwd.
#[tokio::test]
async fn sandboxed_self_check_artifacts_stay_contained() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("child-workdir");
    let host = connect_with(ProcessSandbox {
        env_whitelist: Some(vec!["PATH".into()]),
        cwd: Some(cwd.clone()),
        ..ProcessSandbox::default()
    })
    .await;

    // The check passes and reports where its artifact landed.
    let result: Value = host
        .call(serde_json::json!({ "op": "self_check" }))
        .await
        .expect("self_check op answers");
    assert_eq!(result["passed"], true, "the self-check must pass");

    // The artifact is real, and it sits inside the sandboxed cwd — the
    // only place the sandboxed child can write.
    let probe = result["probe"].as_str().unwrap_or_default().to_string();
    assert!(
        probe.starts_with(&cwd.to_string_lossy().into_owned()),
        "the self-check artifact must stay inside the sandboxed cwd: {probe}"
    );
    assert!(
        std::path::PathBuf::from(&probe).exists(),
        "the self-check must really write its artifact"
    );

    // The parent workspace saw nothing: no trace of the verification
    // outside the sandbox cwd.
    let parent_entries: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != "child-workdir")
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        parent_entries
            .iter()
            .all(|name| !name.contains("self-check-result")),
        "the self-check artifact must not escape the sandbox: {parent_entries:?}"
    );

    host.shutdown().await;
}

/// A child that answers a request with a system frame instead of the
/// normal response must be refused and killed when no broker is installed:
/// there is no sanctioned way to answer it, so the connection poisons and
/// the child tree is terminated (fail-closed — an undeclared access
/// request can never be silently satisfied).
#[tokio::test]
async fn system_frames_without_a_broker_poison_and_kill_the_connection() {
    let host = connect_with(ProcessSandbox::default()).await;
    let error = host
        .call(serde_json::json!({ "op": "system_abuse" }))
        .await
        .expect_err("a system frame with no broker must be refused");
    assert!(
        error.to_string().contains("no broker"),
        "the refusal must name the missing broker: {error}"
    );

    // The connection is poisoned and the child was killed: any further
    // call fails fast, and shutdown reaps the dead child immediately.
    let poisoned = host
        .call(serde_json::json!({ "op": "ping" }))
        .await
        .expect_err("the poisoned connection must refuse further calls");
    assert!(
        poisoned.to_string().contains("poisoned"),
        "the failure must name the poisoned connection: {poisoned}"
    );
    host.shutdown().await;
}

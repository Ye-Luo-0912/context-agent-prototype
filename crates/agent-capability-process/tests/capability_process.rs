//! Process-capability adapter tests: `Capability` served by a separate
//! process over the shared `ProcessHost` (from `agent-process`), driven by
//! the `mock_host` bin. This is the generic shape every process capability
//! reuses — no per-capability stdio adapter, exactly like the
//! context-service adapter.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_capability_process::ProcessCapabilityAdapter;
use agent_contracts::{
    AgentResult, CancellationToken, Capability, CapabilityInvocationContext, CapabilityKind,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ToolCall, ToolRisk, ToolSpec,
};
use agent_process::ProcessHostConfig;
use serde_json::json;

fn manifest_with_program(program: &str) -> CapabilityManifest {
    CapabilityManifest {
        id: "process-demo".into(),
        version: "1.0.0".into(),
        name: "process demo".into(),
        summary: "demo process capability".into(),
        status: CapabilityStatus::Experimental,
        provides: vec![CapabilityKind::Tool],
        permissions: vec!["workspace:read".into()],
        requires: Vec::new(),
        tools: vec![ToolSpec {
            name: "process-demo.invoke".into(),
            description: "invoke the process capability".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
        }],
        lifecycle: CapabilityLifecycle::Lazy,
        transport: CapabilityTransport::Process {
            program: program.into(),
        },
    }
}

#[tokio::test]
async fn process_capability_round_trips_an_invoke_over_the_host() {
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            env: vec![("MOCK_MARKER".into(), "1".into())],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_frame_bytes: 1024 * 1024,
            sandbox: Default::default(),
        },
    ));

    // The manifest declares its tool schemas; the adapter serves them
    // without starting the process.
    let specs = capability.tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "process-demo.invoke");

    capability.start().await.unwrap();
    let output = capability
        .invoke(
            ToolCall {
                id: "c1".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let output = match output {
        CapabilityOutcome::Value(output) => output,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(output.ok);
    assert_eq!(output.call_id, "c1");
    assert!(
        output.model_content.contains("process capability handled"),
        "the child's ToolOutput must cross the boundary: {}",
        output.model_content
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn cancellation_aborts_a_long_running_invoke_and_kills_the_child() {
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            env: vec![("MOCK_MARKER".into(), "1".into())],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: 1024 * 1024,
            sandbox: Default::default(),
        },
    ));
    capability.start().await.unwrap();

    // The mock never answers a `silent` call: without cancellation this
    // would hang until the 30s request deadline. The runtime's cancel
    // token must abort it immediately.
    let cancel = CancellationToken::new();
    let cancel_for_kill = cancel.clone();
    let kill = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_for_kill.cancel();
    });

    let result = capability
        .invoke(
            ToolCall {
                id: "c2".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({"silent": true}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: cancel.clone(),
            },
        )
        .await;
    kill.await.unwrap();
    match result {
        Err(agent_contracts::AgentError::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // Cancellation poisons the connection and kills the child: any further
    // call fails fast instead of riding a dead pipe.
    let after = capability
        .invoke(
            ToolCall {
                id: "c3".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert!(
        after.is_err(),
        "the poisoned connection must refuse further calls"
    );
    let message = after.unwrap_err().to_string();
    assert!(
        message.contains("poisoned") || message.contains("closed"),
        "the failure must name the poisoned connection: {message}"
    );
}

#[tokio::test]
async fn cancellation_terminates_the_child_process() {
    // A heartbeat file the child rewrites every 50 ms is the observable
    // liveness signal: after the cancel, the counter must stop advancing —
    // the whole process tree is dead, not just the pending request.
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("heartbeat.txt");
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            env: vec![
                ("MOCK_MARKER".into(), "1".into()),
                (
                    "MOCK_HEARTBEAT".into(),
                    heartbeat.to_string_lossy().into_owned(),
                ),
            ],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: 1024 * 1024,
            sandbox: Default::default(),
        },
    ));
    capability.start().await.unwrap();

    // Give the heartbeat a moment to start ticking, then confirm it moves.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let ticking = std::fs::read_to_string(&heartbeat).unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_ne!(
        std::fs::read_to_string(&heartbeat).unwrap_or_default(),
        ticking,
        "the heartbeat must advance while the child is alive"
    );

    // Cancel a silent invoke: the adapter must kill the child tree.
    let cancel = CancellationToken::new();
    let cancel_for_kill = cancel.clone();
    let kill = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_for_kill.cancel();
    });
    let result = capability
        .invoke(
            ToolCall {
                id: "c2".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({"silent": true}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: cancel.clone(),
            },
        )
        .await;
    kill.await.unwrap();
    assert!(
        matches!(result, Err(agent_contracts::AgentError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );

    // The heartbeat must freeze: a cancelled capability is a terminated
    // child, not a background process still producing side effects.
    let frozen = std::fs::read_to_string(&heartbeat).unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read_to_string(&heartbeat).unwrap_or_default(),
        frozen,
        "the child must be terminated after cancellation — the heartbeat stopped"
    );
}

#[tokio::test]
async fn strict_sandbox_scrubs_parent_secrets_across_the_wire() {
    // The adapter's production sandbox (the `from_manifest` shape) drops
    // every unlisted parent variable and runs the child in a dedicated
    // cwd. A parent "secret" must be invisible inside the child, and an
    // `echo_env` invoke must come back empty — the sandbox is enforced,
    // not just declared.
    //
    // SAFETY: this test binary is its own process and mutates the
    // environment exactly once, before the child spawns.
    unsafe {
        std::env::set_var("SANDBOX_SECRET", "parent-secret-value");
    }
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            // MOCK_MARKER is the explicit grant that lets the mock serve;
            // SANDBOX_SECRET must NOT be granted here.
            env: vec![("MOCK_MARKER".into(), "1".into())],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_frame_bytes: 1024 * 1024,
            sandbox: agent_process::ProcessSandbox {
                // The same strict shape `from_manifest` builds: only the
                // non-secret platform essentials are inherited, and the
                // child runs in its own working directory. Resource limits
                // are deliberately left off here: RLIMIT_NPROC is a
                // *per-user* ceiling on Linux, and on a busy CI host the
                // user's thread count already exceeds any small limit, so
                // the child could not even start its stdio threads — that
                // dimension is not what this test asserts.
                env_whitelist: Some(vec![
                    "PATH".into(),
                    "SystemRoot".into(),
                    "SystemDrive".into(),
                    "TEMP".into(),
                    "TMP".into(),
                ]),
                cwd: Some(std::env::temp_dir().join("context-agent-capability-sandbox-test")),
                ..agent_process::ProcessSandbox::default()
            },
        },
    ));
    capability.start().await.unwrap();
    let output = capability
        .invoke(
            ToolCall {
                id: "c4".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({"echo_env": "SANDBOX_SECRET"}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let output = match output {
        CapabilityOutcome::Value(output) => output,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        output.model_content.is_empty(),
        "the strict sandbox must scrub unlisted parent env, got: {:?}",
        output.model_content
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn granted_permissions_reach_the_child_intact() {
    // The runtime hands the capability the manifest's granted permissions
    // per invocation; they must arrive at the subprocess unchanged — the
    // child can only know what it may do if the grant actually crossed.
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            env: vec![("MOCK_MARKER".into(), "1".into())],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_frame_bytes: 1024 * 1024,
            sandbox: Default::default(),
        },
    ));
    capability.start().await.unwrap();
    let granted = vec!["workspace:read".to_string(), "process:run".to_string()];
    let output = capability
        .invoke(
            ToolCall {
                id: "c5".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({"echo_permissions": true}),
            },
            CapabilityInvocationContext {
                granted_permissions: granted.clone(),
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let output = match output {
        CapabilityOutcome::Value(output) => output,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert_eq!(
        output.model_content,
        serde_json::to_string(&granted).unwrap(),
        "the granted permission set must cross the boundary intact"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn from_manifest_rejects_non_process_transports() {
    let mut manifest = manifest_with_program("irrelevant");
    manifest.transport = CapabilityTransport::Builtin;
    let error = match ProcessCapabilityAdapter::from_manifest(manifest) {
        Ok(_) => panic!("a builtin transport is not a process capability"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("Process transport"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn invoke_before_start_fails_with_a_clear_error() {
    let program = common::locate_mock_host().expect("mock_host built");
    let capability = ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program.to_string_lossy()),
        ProcessHostConfig {
            program: program.to_string_lossy().into_owned(),
            args: vec!["--serve".into()],
            env: vec![("MOCK_MARKER".into(), "1".into())],
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            max_frame_bytes: 1024 * 1024,
            sandbox: Default::default(),
        },
    );
    let result: AgentResult<_> = capability
        .invoke(
            ToolCall {
                id: "c1".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({}),
            },
            CapabilityInvocationContext {
                granted_permissions: Vec::new(),
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert!(
        result.unwrap_err().to_string().contains("not started"),
        "invoking an unstarted process capability must fail fast"
    );
}

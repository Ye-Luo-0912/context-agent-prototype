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
    AgentError, AgentResult, CancellationToken, Capability, CapabilityInvocationContext,
    CapabilityKind, CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, Effect, EffectCommitError, ToolCall, ToolRisk, ToolSpec, WORKSPACE_WRITE,
    WorkspaceHandle,
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
            output_budget: None,
        }],
        lifecycle: CapabilityLifecycle::Lazy,
        transport: CapabilityTransport::Process {
            program: program.into(),
        },
    }
}

/// A manifest for a write-capable process: declares `workspace:write` and a
/// non-ReadOnly tool, so the adapter accepts it (risk is derived from
/// declared authority, never self-declared).
fn write_manifest_with_program(program: &str) -> CapabilityManifest {
    let mut manifest = manifest_with_program(program);
    manifest.permissions = vec![WORKSPACE_WRITE.into()];
    manifest.tools = vec![ToolSpec {
        name: "process-demo.invoke".into(),
        description: "invoke the process capability".into(),
        input_schema: json!({"type": "object"}),
        risk: ToolRisk::WorkspaceWrite,
        output_budget: None,
    }];
    manifest
}

/// A test double for the confined workspace handle: a real temp directory
/// whose `prepare_write` returns an effect that writes on commit. The
/// wire-effect integration test asserts the staged mutation lands exactly
/// like a builtin tool's `PreparedEffect` — the double only stands in for
/// the runtime's journaled handle, the adapter path is the code under test.
struct TestWorkspace {
    root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl WorkspaceHandle for TestWorkspace {
    fn root(&self) -> &std::path::Path {
        &self.root
    }

    async fn resolve(&self, relative: &str) -> AgentResult<std::path::PathBuf> {
        Ok(self.root.join(relative))
    }

    async fn read(&self, relative: &str) -> AgentResult<Vec<u8>> {
        std::fs::read(self.root.join(relative)).map_err(|e| AgentError::Io(e.to_string()))
    }

    async fn write(&self, relative: &str, content: &[u8]) -> AgentResult<()> {
        std::fs::write(self.root.join(relative), content).map_err(|e| AgentError::Io(e.to_string()))
    }

    async fn prepare_write(&self, relative: &str, content: &[u8]) -> AgentResult<Box<dyn Effect>> {
        Ok(Box::new(TestWriteEffect {
            path: self.root.join(relative),
            content: content.to_vec(),
        }))
    }
}

/// The staged write effect the test double returns: commit writes the file,
/// rollback leaves it untouched.
struct TestWriteEffect {
    path: std::path::PathBuf,
    content: Vec<u8>,
}

#[async_trait::async_trait]
impl Effect for TestWriteEffect {
    fn describe(&self) -> String {
        format!("test write to {}", self.path.display())
    }

    async fn commit(self: Box<Self>) -> Result<(), EffectCommitError> {
        std::fs::write(&self.path, &self.content)
            .map_err(|e| EffectCommitError::NotApplied(AgentError::Io(e.to_string())))
    }

    async fn rollback(self: Box<Self>, _reason: &str) {}
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

    // The heartbeat thread lives inside the child and rewrites the file
    // every 50 ms, but on a busy host the child (or its thread) can be
    // scheduled late. Poll until the counter visibly advances instead of
    // racing two fixed-sleep reads.
    let baseline = std::fs::read_to_string(&heartbeat).unwrap_or_default();
    let mut saw_advance = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if std::fs::read_to_string(&heartbeat).unwrap_or_default() != baseline {
            saw_advance = true;
            break;
        }
    }
    assert!(
        saw_advance,
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

#[test]
fn from_manifest_rejects_ids_that_could_escape_a_path() {
    // The id is embedded in the capability's private working directory and
    // protocol routes; anything outside the conservative grammar is a
    // path/route injection risk and must be refused at the adapter too
    // (the registry enforces the same rule).
    for bad in ["../escape", "a/b", "a\\b", "Uppercase", "a b", "-leading"] {
        let mut manifest = manifest_with_program("irrelevant");
        manifest.id = bad.into();
        let error = match ProcessCapabilityAdapter::from_manifest(manifest) {
            Ok(_) => panic!("id {bad:?} must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("capability id"),
            "id {bad:?}: the refusal must name the id rule, got: {error}"
        );
    }
}

#[test]
fn from_manifest_rejects_readonly_tools_on_write_capabilities() {
    // A process that can write the workspace must not auto-allow through a
    // ReadOnly tool at the approval gate — risk is derived from declared
    // authority, never self-declared. The adapter refuses the combination
    // even if the manifest never reached the registry.
    let mut manifest = manifest_with_program("irrelevant");
    manifest.permissions = vec![agent_contracts::WORKSPACE_WRITE.into()];
    let error = match ProcessCapabilityAdapter::from_manifest(manifest) {
        Ok(_) => panic!("a write-permissioned process must not self-declare ReadOnly"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("ReadOnly"),
        "the refusal must name the self-declared risk: {error}"
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

#[tokio::test]
async fn staged_wire_write_returns_an_effect_request() {
    // The mock declares a workspace-write wire effect; the adapter must
    // validate the grant, stage it through the confined handle, and hand
    // the runtime an `EffectRequest` — the child never mutates anything
    // itself, it declares intent. Nothing lands until the runtime commits
    // the effect behind the generation fence.
    let dir = tempfile::tempdir().unwrap();
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        write_manifest_with_program(&program.to_string_lossy()),
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

    let workspace: Arc<dyn WorkspaceHandle> = Arc::new(TestWorkspace {
        root: dir.path().to_path_buf(),
    });
    let outcome = capability
        .invoke(
            ToolCall {
                id: "c6".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({
                    "stage_write": {
                        "path": "staged.txt",
                        "content": "staged content",
                    }
                }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec![WORKSPACE_WRITE.into()],
                workspace: Some(workspace.clone()),
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let (output, effect) = match outcome {
        CapabilityOutcome::EffectRequest { output, effect } => (output, effect),
        other => panic!("expected an EffectRequest, got {other:?}"),
    };
    assert!(output.ok);
    assert!(
        output.model_content.contains("process capability handled"),
        "the child's ToolOutput must cross the boundary: {}",
        output.model_content
    );

    // The mutation is staged, not applied: the file must not exist until
    // the runtime commits the effect.
    assert!(
        !dir.path().join("staged.txt").exists(),
        "the wire effect must be staged, never applied by the child or the adapter"
    );
    effect.commit().await.expect("the staged effect commits");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("staged.txt")).unwrap(),
        "staged content",
        "the staged bytes must land exactly as declared"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn wire_write_without_the_grant_is_refused() {
    // The child declared a write intent, but this invocation was not
    // granted `workspace:write`: the adapter must refuse before anything
    // is staged. Declared permission sets are enforced, never assumed —
    // an over-granted effect must not reach the workspace handle.
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        write_manifest_with_program(&program.to_string_lossy()),
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
    let result = capability
        .invoke(
            ToolCall {
                id: "c7".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({
                    "stage_write": {
                        "path": "x.txt",
                        "content": "x",
                    }
                }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("without 'workspace:write' permission"),
        "the refusal must name the missing grant: {message}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn wire_write_without_a_workspace_handle_is_refused() {
    // Even with the permission string present, a capability that never
    // received a confined workspace handle cannot stage a write — the
    // handle is the enforcement, not the permission string.
    let program = common::locate_mock_host().expect("mock_host built");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        write_manifest_with_program(&program.to_string_lossy()),
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
    let result = capability
        .invoke(
            ToolCall {
                id: "c8".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({
                    "stage_write": {
                        "path": "x.txt",
                        "content": "x",
                    }
                }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec![WORKSPACE_WRITE.into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("no workspace handle"),
        "the refusal must name the missing handle: {message}"
    );
    capability.stop().await.unwrap();
}

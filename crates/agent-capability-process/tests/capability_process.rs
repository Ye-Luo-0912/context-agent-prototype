//! Process-capability adapter tests: `Capability` served by a separate
//! process over the shared `ProcessHost` (from `agent-process`), driven by
//! the `mock_host` bin. This is the generic shape every process capability
//! reuses — no per-capability stdio adapter, exactly like the
//! context-service adapter.

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_capability_process::{McpCapabilityAdapter, McpServerDecl, ProcessCapabilityAdapter};
use agent_contracts::{
    AgentError, AgentResult, BoundedRead, CancellationToken, Capability,
    CapabilityInvocationContext, CapabilityKind, CapabilityLifecycle, CapabilityManifest,
    CapabilityOutcome, CapabilityStatus, CapabilityTransport, Effect, EffectDurability,
    EffectReceipt, ToolCall, ToolRisk, ToolSpec, WORKSPACE_WRITE, WorkspaceHandle,
};
use agent_platform_protocol::{ActiveFeatures, FEATURE_LEGACY_INVOKE_OUTPUT};
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
            roles: Vec::new(),
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
        roles: Vec::new(),
    }];
    manifest
}

/// A test double for the confined workspace handle. Tests may attach a
/// counter to prove a rejected process wire effect never reached
/// `prepare_write`; the returned effect remains useful for ordinary handle
/// tests outside the quarantined process path.
struct TestWorkspace {
    root: std::path::PathBuf,
    prepare_calls: Option<Arc<AtomicUsize>>,
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

    async fn read_bounded(&self, relative: &str, max_bytes: usize) -> AgentResult<BoundedRead> {
        use std::io::Read;

        let mut file = std::fs::File::open(self.root.join(relative))
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let byte_len = file
            .metadata()
            .map_err(|e| AgentError::Io(e.to_string()))?
            .len();
        let mut content = Vec::new();
        file.by_ref()
            .take(max_bytes as u64)
            .read_to_end(&mut content)
            .map_err(|e| AgentError::Io(e.to_string()))?;
        Ok(BoundedRead {
            content,
            byte_len,
            truncated: byte_len > max_bytes as u64,
        })
    }

    async fn write(&self, relative: &str, content: &[u8]) -> AgentResult<()> {
        std::fs::write(self.root.join(relative), content).map_err(|e| AgentError::Io(e.to_string()))
    }

    async fn prepare_write(&self, relative: &str, content: &[u8]) -> AgentResult<Box<dyn Effect>> {
        if let Some(calls) = &self.prepare_calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Box::new(TestWriteEffect {
            path: self.root.join(relative),
            content: content.to_vec(),
        }))
    }
}

/// A broker test double that fails if the unbounded compatibility API is
/// called. This proves the process boundary selects the bounded primitive,
/// rather than merely checking that its returned JSON happens to be short.
struct BoundedOnlyWorkspace {
    root: std::path::PathBuf,
    bounded_reads: AtomicUsize,
}

#[async_trait::async_trait]
impl WorkspaceHandle for BoundedOnlyWorkspace {
    fn root(&self) -> &std::path::Path {
        &self.root
    }

    async fn resolve(&self, relative: &str) -> AgentResult<std::path::PathBuf> {
        Ok(self.root.join(relative))
    }

    async fn read(&self, _relative: &str) -> AgentResult<Vec<u8>> {
        Err(AgentError::InvalidRequest(
            "unbounded read must not be called by the broker".into(),
        ))
    }

    async fn read_bounded(&self, relative: &str, max_bytes: usize) -> AgentResult<BoundedRead> {
        self.bounded_reads.fetch_add(1, Ordering::Relaxed);
        use std::io::Read;

        let mut file = std::fs::File::open(self.root.join(relative))
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let byte_len = file
            .metadata()
            .map_err(|e| AgentError::Io(e.to_string()))?
            .len();
        let mut content = Vec::new();
        file.by_ref()
            .take(max_bytes as u64)
            .read_to_end(&mut content)
            .map_err(|e| AgentError::Io(e.to_string()))?;
        Ok(BoundedRead {
            content,
            byte_len,
            truncated: byte_len > max_bytes as u64,
        })
    }

    async fn write(&self, _relative: &str, _content: &[u8]) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(
            "test workspace is read-only".into(),
        ))
    }

    async fn prepare_write(
        &self,
        _relative: &str,
        _content: &[u8],
    ) -> AgentResult<Box<dyn Effect>> {
        Err(AgentError::InvalidRequest(
            "test workspace is read-only".into(),
        ))
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

    async fn commit(self: Box<Self>) -> EffectReceipt {
        match std::fs::write(&self.path, &self.content) {
            Ok(()) => EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: None,
            },
            Err(e) => EffectReceipt::NotApplied {
                error: e.to_string(),
            },
        }
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
                // The mock deliberately reports the legacy canned id `c1`;
                // the trusted adapter must bind the output to this request.
                id: "requested-call".into(),
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
    assert_eq!(output.call_id, "requested-call");
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
async fn mcp_cancel_after_spawn_terminates_the_server_tree() {
    // The MCP adapter owns its server child for the whole connection: a
    // cancelled invoke must kill and reap the tree (the heartbeat thread
    // inside the server stops), and the next invoke must come back through
    // a fresh connection — the poisoned client is replaced, never reused.
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("mcp-heartbeat.txt");
    let adapter = McpCapabilityAdapter::connect(
        McpServerDecl {
            id: "mock-mcp".into(),
            version: "1.0.0".into(),
            name: "mock mcp".into(),
            summary: "mock server for tests".into(),
            program: common::locate_mcp_mock_server()
                .expect("mcp_mock_server built")
                .to_string_lossy()
                .into_owned(),
            args: Vec::new(),
            permissions: vec!["workspace:read".into()],
            extra_write_roots: vec![dir.path().to_path_buf()],
        },
        ToolRisk::ReadOnly,
        Duration::from_secs(10),
        1024 * 1024,
    )
    .await
    .expect("connect + discover succeeds");

    // `mock.echo` with a `heartbeat` path writes READY on the tools/call
    // thread, then ticks; `hang: true` never answers, so only the runtime's
    // cancel token can end the call. Wait until READY exists (same 3s
    // fail-closed bound as before — not a longer timeout), then cancel.
    let capability: Arc<dyn Capability> = Arc::new(adapter);
    let cancel = CancellationToken::new();
    let invoke_cancel = cancel.clone();
    let capability_for_invoke = capability.clone();
    let heartbeat_for_invoke = heartbeat.clone();
    let invoke_task = tokio::spawn(async move {
        capability_for_invoke
            .invoke(
                ToolCall {
                    id: "c20".into(),
                    name: "mock.echo".into(),
                    arguments: json!({
                        "heartbeat": heartbeat_for_invoke.to_string_lossy(),
                        "hang": true,
                    }),
                },
                CapabilityInvocationContext {
                    granted_permissions: vec!["workspace:read".into()],
                    workspace: None,
                    artifacts: None,
                    cancel: invoke_cancel,
                },
            )
            .await
    });

    let mut ready = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let body = std::fs::read_to_string(&heartbeat).unwrap_or_default();
        if body == "ready" || body.parse::<u64>().is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ready,
        "MCP invoke never reached READY (heartbeat file missing)"
    );

    cancel.cancel();
    let result = invoke_task.await.unwrap();
    assert!(
        matches!(result, Err(agent_contracts::AgentError::Cancelled)),
        "a cancelled MCP invoke must surface Cancelled, got {result:?}"
    );

    // The server tree is dead: the heartbeat must freeze.
    let frozen = std::fs::read_to_string(&heartbeat).unwrap_or_default();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read_to_string(&heartbeat).unwrap_or_default(),
        frozen,
        "the MCP server tree must be terminated after cancellation — the heartbeat stopped"
    );

    // The poisoned client is replaced on the next invoke: a fresh server
    // tree connects and serves again, so a cancelled capability is not a
    // dead capability.
    let outcome = capability
        .invoke(
            ToolCall {
                id: "c21".into(),
                name: "mock.add".into(),
                arguments: json!({"a": 1, "b": 2}),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .expect("a fresh connection must replace the poisoned one");
    match outcome {
        CapabilityOutcome::Value(output) => assert_eq!(output.model_content, "3"),
        other => panic!("mock.add must return a plain value, got {other:?}"),
    }
    capability.stop().await.unwrap();
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
async fn nonempty_wire_effects_fail_closed_before_workspace_mutation() {
    // The current process wire effect has no host-verifiable canonical
    // actual intent. Even a write-capable invocation with a confined handle
    // must therefore fail before `prepare_write`, leaving existing workspace
    // state byte-for-byte unchanged.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("staged.txt"), "original content").unwrap();
    let prepare_calls = Arc::new(AtomicUsize::new(0));
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
            sandbox: Default::default(),
        },
    ));
    capability.start().await.unwrap();

    let workspace: Arc<dyn WorkspaceHandle> = Arc::new(TestWorkspace {
        root: dir.path().to_path_buf(),
        prepare_calls: Some(prepare_calls.clone()),
    });
    let result = capability
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
        .await;
    let error = result.expect_err("a non-empty process wire effect must be quarantined");
    let message = error.to_string();
    assert!(
        message.contains("process wire effects are disabled")
            && message.contains("no workspace mutation was staged"),
        "the refusal must name the temporary safety gate and its no-mutation result: {message}"
    );
    assert_eq!(
        prepare_calls.load(Ordering::SeqCst),
        0,
        "the adapter must reject before asking the workspace to stage anything"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("staged.txt")).unwrap(),
        "original content",
        "a rejected wire effect must not replace existing workspace state"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn wire_effect_quarantine_precedes_legacy_grant_matching() {
    // Wire-effect quarantine precedes legacy permission matching: an
    // ungranted child gets the same fail-closed result and no staging path.
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
        message.contains("process wire effects are disabled")
            && message.contains("no workspace mutation was staged"),
        "the refusal must name the wire-effect quarantine: {message}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn wire_effect_quarantine_does_not_require_a_workspace_handle() {
    // A permission word without a workspace handle also cannot reach a
    // staging branch; the temporary wire-effect quarantine is unconditional.
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
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
        message.contains("process wire effects are disabled")
            && message.contains("no workspace mutation was staged"),
        "the refusal must name the wire-effect quarantine: {message}"
    );
    capability.stop().await.unwrap();
}

/// A started read-only process capability on the mock host, ready for
/// invoke. The sandbox dimensions are covered by the dedicated sandbox
/// tests; these tests exercise the mid-invoke broker.
async fn started_readonly_capability() -> Arc<dyn Capability> {
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
            max_call_bytes: 4 * 1024 * 1024,
            max_system_answer_bytes: 512 * 1024,
            offered_features: Default::default(),
            sandbox: Default::default(),
        },
    ));
    capability.start().await.unwrap();
    capability
}

/// One brokered `fs.read` invoke against the mock: the child issues a
/// mid-invoke `{"system": "fs.read", "path": ...}` frame, the adapter's
/// broker answers it, and the mock reports the outcome in `model_content`
/// as `FS_READ:<content>` or `FS_REFUSED:<error>`.
async fn invoke_broker_read(
    capability: &Arc<dyn Capability>,
    workspace: &Arc<dyn WorkspaceHandle>,
    path: &str,
) -> String {
    let output = capability
        .invoke(
            ToolCall {
                id: "c9".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_fs_read": path }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: Some(workspace.clone()),
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    }
}

#[tokio::test]
async fn brokered_fs_read_serves_files_inside_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "hello broker").unwrap();
    let workspace: Arc<dyn WorkspaceHandle> = Arc::new(TestWorkspace {
        root: dir.path().to_path_buf(),
        prepare_calls: None,
    });

    let capability = started_readonly_capability().await;
    let model_content = invoke_broker_read(&capability, &workspace, "notes.txt").await;
    assert!(
        model_content.contains("FS_READ:hello broker"),
        "the brokered read must return the workspace file's content: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn brokered_fs_read_is_bounded_for_large_files() {
    // The control plane is not a file transport: a file bigger than the
    // broker's read bound must come back as the bounded head of the file,
    // with the truncation metadata naming the original size — never a full
    // base64 copy through the JSON pipe.
    let dir = tempfile::tempdir().unwrap();
    let size = agent_capability_process::BROKER_FS_READ_MAX_BYTES + 48 * 1024;
    let content = vec![b'a'; size];
    std::fs::write(dir.path().join("big.bin"), &content).unwrap();
    let workspace = Arc::new(BoundedOnlyWorkspace {
        root: dir.path().to_path_buf(),
        bounded_reads: AtomicUsize::new(0),
    });
    let workspace_handle: Arc<dyn WorkspaceHandle> = workspace.clone();

    let capability = started_readonly_capability().await;
    let model_content = invoke_broker_read(&capability, &workspace_handle, "big.bin").await;
    let payload = model_content
        .strip_prefix("FS_READ:")
        .expect("the mock must report a served read");
    let (payload, meta) = payload.split_once('\n').unwrap_or((payload, ""));
    assert_eq!(
        payload.len(),
        agent_capability_process::BROKER_FS_READ_MAX_BYTES,
        "the broker must serve only the bounded head of the file"
    );
    assert_eq!(
        payload.as_bytes(),
        &content[..agent_capability_process::BROKER_FS_READ_MAX_BYTES],
        "the served prefix must be the file's head, not an arbitrary slice"
    );
    assert!(
        meta.contains(&format!("byte_len={size}")) && meta.contains("truncated=true"),
        "the truncation metadata must name the original size: {meta}"
    );
    assert_eq!(
        workspace.bounded_reads.load(Ordering::Relaxed),
        1,
        "the broker must use exactly one bounded read and never call full read"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn brokered_fs_read_refuses_absolute_and_escaping_paths() {
    let dir = tempfile::tempdir().unwrap();
    let workspace: Arc<dyn WorkspaceHandle> = Arc::new(TestWorkspace {
        root: dir.path().to_path_buf(),
        prepare_calls: None,
    });

    let capability = started_readonly_capability().await;
    // An absolute/rooted path is refused before the workspace handle ever
    // sees it — even on platforms where `/etc/passwd` is not "absolute".
    let model_content = invoke_broker_read(&capability, &workspace, "/etc/passwd").await;
    assert!(
        model_content.contains("FS_REFUSED:") && model_content.contains("absolute"),
        "an absolute path must be refused: {model_content}"
    );
    // A `..` escape is refused the same way.
    let model_content = invoke_broker_read(&capability, &workspace, "../secret.txt").await;
    assert!(
        model_content.contains("FS_REFUSED:") && model_content.contains("escaping"),
        "a `..` escape must be refused: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn brokered_fs_read_without_the_grant_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), "top secret").unwrap();
    let workspace: Arc<dyn WorkspaceHandle> = Arc::new(TestWorkspace {
        root: dir.path().to_path_buf(),
        prepare_calls: None,
    });

    let capability = started_readonly_capability().await;
    // This invocation was not granted `workspace:read`: even though the
    // file is inside the workspace, the broker must refuse before the
    // handle could be touched.
    let output = capability
        .invoke(
            ToolCall {
                id: "c10".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_fs_read": "secret.txt" }),
            },
            CapabilityInvocationContext {
                granted_permissions: Vec::new(),
                workspace: Some(workspace),
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("FS_REFUSED:") && model_content.contains("workspace:read"),
        "the refusal must name the missing permission: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_system_ops_are_refused() {
    let capability = started_readonly_capability().await;
    let output = capability
        .invoke(
            ToolCall {
                id: "c11".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_unknown_system": true }),
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
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("FS_REFUSED:") && model_content.contains("unknown system op"),
        "an undeclared system op must be refused: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn brokered_network_requests_are_refused_by_default() {
    // The permission vocabulary has no network word at all, so a network
    // system request is denied by default — with an explicit, nameable
    // refusal instead of a silent fall-through.
    let capability = started_readonly_capability().await;
    let output = capability
        .invoke(
            ToolCall {
                id: "c12".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_net_fetch": true }),
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
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("NET_REFUSED:")
            && model_content.contains("net.fetch")
            && model_content.contains("deny-by-default"),
        "a network op must be refused with the explicit policy: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn network_requests_are_refused_even_with_a_networkish_grant() {
    // No network permission exists, so even a grant string that *looks*
    // like one (from a misdeclared manifest, or a future over-eager
    // registry) must not unlock the network: the broker never consults a
    // grant for these ops — there is nothing to grant.
    let capability = started_readonly_capability().await;
    let output = capability
        .invoke(
            ToolCall {
                id: "c13".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_net_fetch": true }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["net:fetch".into(), "workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("NET_REFUSED:") && model_content.contains("net.fetch"),
        "a networkish grant must not unlock the network: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn a_refused_system_request_does_not_poison_the_connection() {
    // A broker *refusal* is an answer, not a connection failure: after an
    // unknown-op refusal the same capability must still serve normal
    // invokes. Only a broker-less frame or a flood is fatal to the
    // connection.
    let capability = started_readonly_capability().await;
    let output = capability
        .invoke(
            ToolCall {
                id: "c14".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_unknown_system": true }),
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
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("FS_REFUSED:") && model_content.contains("unknown system op"),
        "the first call must surface the refusal: {model_content}"
    );

    // Same capability, same connection: a plain invoke still works.
    let output = capability
        .invoke(
            ToolCall {
                id: "c15".into(),
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
    let model_content = match output {
        CapabilityOutcome::Value(output) => output.model_content,
        other => panic!("the wire only carries plain values, got: {other:?}"),
    };
    assert!(
        model_content.contains("process capability handled"),
        "a refused system request must not break the connection: {model_content}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn a_system_request_flood_poisons_and_kills_the_connection() {
    // The host caps mid-invoke system frames per call
    // (`MAX_SYSTEM_REQUESTS_PER_CALL`): a child that keeps asking must be
    // refused and killed, not served forever — the flood is an
    // availability attack on the host.
    let capability = started_readonly_capability().await;
    let result = capability
        .invoke(
            ToolCall {
                id: "c16".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "ask_fs_flood": true }),
            },
            CapabilityInvocationContext {
                granted_permissions: vec!["workspace:read".into()],
                workspace: None,
                artifacts: None,
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let message = match result {
        Err(error) => error.to_string(),
        Ok(_) => panic!("a system flood must fail the invoke"),
    };
    assert!(
        message.contains("too many system requests") || message.contains("poisoned"),
        "the flood must be refused with the cap or poison named: {message}"
    );

    // The connection is dead: a follow-up invoke fails fast instead of
    // riding a dead pipe.
    let after = capability
        .invoke(
            ToolCall {
                id: "c17".into(),
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
    let message = after.unwrap_err().to_string();
    assert!(
        message.contains("poisoned"),
        "the flood must poison the connection: {message}"
    );
    capability.stop().await.unwrap();
}

fn mock_process_config(program: &str) -> ProcessHostConfig {
    ProcessHostConfig {
        program: program.to_string(),
        args: vec!["--serve".into()],
        env: vec![("MOCK_MARKER".into(), "1".into())],
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        max_frame_bytes: 1024 * 1024,
        max_call_bytes: 4 * 1024 * 1024,
        max_system_answer_bytes: 512 * 1024,
        offered_features: Default::default(),
        sandbox: Default::default(),
    }
}

fn invoke_ctx() -> CapabilityInvocationContext {
    CapabilityInvocationContext {
        granted_permissions: vec!["workspace:read".into()],
        workspace: None,
        artifacts: None,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn plain_tool_output_is_rejected_without_legacy_negotiation() {
    let program = common::locate_mock_host().expect("mock_host built");
    let program = program.to_string_lossy().into_owned();
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program),
        mock_process_config(&program),
    ));
    capability.start().await.unwrap();
    let error = capability
        .invoke(
            ToolCall {
                id: "requested-call".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "legacy_plain": true }),
            },
            invoke_ctx(),
        )
        .await
        .expect_err("plain ToolOutput must stay closed without negotiation");
    assert!(
        error.to_string().contains("legacy.invoke-output.v1"),
        "refusal must name the feature: {error}"
    );
    capability.stop().await.unwrap();
}

#[tokio::test]
async fn plain_tool_output_is_accepted_after_legacy_negotiation() {
    let program = common::locate_mock_host().expect("mock_host built");
    let program = program.to_string_lossy().into_owned();
    let mut config = mock_process_config(&program);
    config.offered_features =
        ActiveFeatures::new(vec![FEATURE_LEGACY_INVOKE_OUTPUT.into()]).expect("known feature");
    let capability: Arc<dyn Capability> = Arc::new(ProcessCapabilityAdapter::with_config(
        manifest_with_program(&program),
        config,
    ));
    capability.start().await.unwrap();
    let output = capability
        .invoke(
            ToolCall {
                id: "requested-call".into(),
                name: "process-demo.invoke".into(),
                arguments: json!({ "legacy_plain": true }),
            },
            invoke_ctx(),
        )
        .await
        .unwrap();
    let output = match output {
        CapabilityOutcome::Value(output) => output,
        other => panic!("expected a value outcome, got: {other:?}"),
    };
    assert_eq!(output.call_id, "requested-call");
    assert_eq!(output.tool_name, "process-demo.invoke");
    capability.stop().await.unwrap();
}

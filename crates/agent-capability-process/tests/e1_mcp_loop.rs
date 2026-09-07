//! E1 closed loop over a REAL MCP server process (the workspace's
//! `mcp_mock_server` bin): configuration → discovery → registration →
//! on-demand load → invocation with a bounded result → shutdown that kills
//! and reaps the server tree.
//!
//! The loop needs no RuntimeActor special-casing: the model surface only
//! carries `capability.manage` until a capability tool is explicitly
//! loaded, the invocation result enters as ordinary bounded output, and
//! compose shutdown settles the server child — observable through the
//! mock's heartbeat file, whose ticker stops once the tree is dead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_capability_process::McpServerDecl;
use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

/// Scripted model driving the four rounds of the loop and recording the
/// tool names it was offered each round.
struct LoopModel {
    step: AtomicUsize,
    offered_tools: std::sync::Mutex<Vec<Vec<String>>>,
    last_messages: std::sync::Mutex<Vec<String>>,
    heartbeat_path: String,
}

impl LoopModel {
    fn new(heartbeat_path: &str) -> Self {
        Self {
            step: AtomicUsize::new(0),
            offered_tools: std::sync::Mutex::new(Vec::new()),
            last_messages: std::sync::Mutex::new(Vec::new()),
            heartbeat_path: heartbeat_path.to_owned(),
        }
    }

    fn take_step(&self) -> usize {
        self.step.fetch_add(1, Ordering::SeqCst)
    }

    fn offered(&self, round: usize) -> Vec<String> {
        self.offered_tools
            .lock()
            .unwrap()
            .get(round)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl ModelTransport for LoopModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calls: true,
            max_output_tokens: 2048,
            context_window: None,
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.offered_tools
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|spec| spec.name.clone()).collect());
        if let Some(last) = request.messages.last() {
            self.last_messages
                .lock()
                .unwrap()
                .push(format!("{:?}: {}", last.role, last.content));
        }
        let plain = |content: &str| {
            Ok(ModelOutput {
                content: content.into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        };
        match self.take_step() {
            // Round 1: discover the capability. Before any load, the surface
            // carries the control tool but NOT the server's tools.
            0 => {
                assert!(
                    !self.offered(0).iter().any(|name| name == "mock.echo"),
                    "an unloaded capability tool must not be on the surface: {:?}",
                    self.offered(0)
                );
                Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![agent_contracts::ToolCall {
                        id: "e1-search".into(),
                        name: "capability.manage".into(),
                        arguments: json!({"op": "search", "query": "mock"}),
                    }],
                    usage: Default::default(),
                })
            }
            // Round 2: load the one tool this task needs.
            1 => Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![agent_contracts::ToolCall {
                    id: "e1-load".into(),
                    name: "capability.manage".into(),
                    arguments: json!({"op": "load", "name": "mock.echo"}),
                }],
                usage: Default::default(),
            }),
            // Round 3: the loaded tool is now on the surface and invocable;
            // its bounded result comes back as ordinary output.
            2 => {
                assert!(
                    self.offered(2).iter().any(|name| name == "mock.echo"),
                    "the loaded capability tool must be on the surface: {:?}; messages: {:?}",
                    self.offered(2),
                    self.last_messages.lock().unwrap()
                );
                Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![agent_contracts::ToolCall {
                        id: "e1-invoke".into(),
                        name: "mock.echo".into(),
                        arguments: json!({
                            "text": "e2e-ping",
                            "heartbeat": self.heartbeat_path,
                        }),
                    }],
                    usage: Default::default(),
                })
            }
            // Round 4: end the turn.
            _ => plain("[scripted] e1 loop finished"),
        }
    }
}

fn mock_server_program() -> std::path::PathBuf {
    let name = if cfg!(windows) {
        "mcp_mock_server.exe"
    } else {
        "mcp_mock_server"
    };
    let current = std::env::current_exe().expect("test exe path");
    agent_process::probe_siblings(&current, name).expect("mcp_mock_server built")
}

#[tokio::test]
async fn e1_loop_config_discover_load_invoke_bounded_result_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let heartbeat_path = temp.path().join("heartbeat.txt");
    let workspace = Workspace::open(temp.path()).await.unwrap();

    let model = Arc::new(LoopModel::new(&heartbeat_path.to_string_lossy()));
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        model: model.clone(),
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: true,
        journal: None,
        artifact_store: None,
        output_broker: None,
        max_tool_rounds: Some(8),
        project_task_progress: false,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: None,
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
        // Harness composition: no watchdog dispatch in this executable.
        host_death_watchdog: false,
        mcp_servers: vec![McpServerDecl {
            id: "mock-mcp".into(),
            version: "1.0.0".into(),
            name: "mock mcp".into(),
            summary: "real mock server process".into(),
            program: mock_server_program().to_string_lossy().into_owned(),
            args: Vec::new(),
            permissions: vec!["workspace:read".into()],
            // The heartbeat file lives in the temp dir: the sandbox only
            // writes the private cwd unless a root is declared explicitly.
            extra_write_roots: vec![temp.path().to_path_buf()],
        }],
        plugins: None,
    })
    .await
    .expect("compose with a discoverable MCP server succeeds");

    let mut events = composed.subscribe();
    composed.instance.start().await.unwrap();
    composed
        .handle()
        .user_message("run the e1 loop".into())
        .await
        .unwrap();

    // The turn runs all four scripted rounds and completes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the turn finishes in time")
            .expect("event stream alive");
        if matches!(event.event, RuntimeEvent::TurnCompleted) {
            break;
        }
    }

    // The invocation actually reached the server: the heartbeat file was
    // written and its ticker is running.
    let read_heartbeat = || -> Option<u64> {
        std::fs::read_to_string(&heartbeat_path)
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    let mut ticker_ran = false;
    for _ in 0..40 {
        if read_heartbeat().is_some() {
            ticker_ran = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        ticker_ran,
        "the invoked tool must have started the server's heartbeat ticker; messages: {:?}",
        composed_model_messages(&model)
    );

    // Shutdown: the ordered teardown stops the capability, and the mock's
    // heartbeat ticker stops with its process tree. Two spaced reads after
    // shutdown must agree — a live ticker always moves the counter.
    composed.shutdown().await.unwrap();
    let after_1 = read_heartbeat();
    std::thread::sleep(Duration::from_millis(300));
    let after_2 = read_heartbeat();
    assert_eq!(
        after_1, after_2,
        "the server tree must be dead after shutdown: the heartbeat ticker stops"
    );
}

fn composed_model_messages(model: &LoopModel) -> Vec<String> {
    model.last_messages.lock().unwrap().clone()
}

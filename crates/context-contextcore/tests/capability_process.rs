//! Process-capability adapter tests: `Capability` served by a separate
//! process over the shared `ProcessHost`, driven by the `mock_host` test
//! target. This is the generic shape every process capability reuses — no
//! per-capability stdio adapter, exactly like the context-service adapter.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, CancellationToken, Capability, CapabilityInvocationContext, CapabilityKind,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ToolCall, ToolRisk, ToolSpec,
};
use context_contextcore::{ProcessCapabilityAdapter, ProcessHostConfig};
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

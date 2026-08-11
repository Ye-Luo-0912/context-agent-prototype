//! Conformance of the builtin tool catalog against the shared harness:
//! schema contract for every known tool, output envelope for representative
//! executions (through the trusted broker), path confinement for workspace
//! tools, and the surface/lifecycle rules. A violation here is a regression
//! in the tool ecosystem contract, not a style nit.

use std::path::Path;
use std::sync::Arc;

use agent_conformance::{
    check_catalog, check_error_envelope, check_output_envelope, check_schema_contract,
};
use agent_contracts::{
    CancellationToken, OutputBroker, RunId, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolOutcome, ToolOutput,
};
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use serde_json::{Value, json};
use tool_runtime::BuiltinToolDispatcher;

fn request(name: &str, arguments: Value) -> ToolExecutionRequest {
    ToolExecutionRequest {
        run_id: RunId::new(),
        call: ToolCall {
            id: "conformance".into(),
            name: name.into(),
            arguments,
        },
        cancel: CancellationToken::new(),
    }
}

/// Bound every outcome variant through the trusted broker (the kernel does
/// this before any `ToolOutcome` reaches the actor) and roll back staged
/// effects so the conformance run never mutates the seeded workspace.
async fn bound_value(
    outcome: ToolOutcome,
    broker: &WorkspaceOutputBroker,
    run_id: RunId,
) -> ToolOutput {
    match outcome {
        ToolOutcome::Value(output) => broker.bound(run_id, None, output).await,
        ToolOutcome::PreparedEffect { output, effect } => {
            effect.rollback("conformance run").await;
            broker.bound(run_id, None, output).await
        }
        ToolOutcome::RuntimeDirective { output, .. } => broker.bound(run_id, None, output).await,
        ToolOutcome::EngineQuery { output, .. } => broker.bound(run_id, None, output).await,
    }
}

fn seed(workspace_root: &Path) {
    let src = workspace_root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();
}

/// One representative call per tool, chosen to exercise a real execution
/// (or a real failure path — git tools outside a repository) rather than a
/// mocked stub.
fn representative_calls() -> Vec<(String, Value)> {
    vec![
        ("fs.list".into(), json!({"path": "."})),
        (
            "fs.read".into(),
            json!({"path": "src/main.rs", "start_line": 1, "end_line": 1}),
        ),
        (
            "search.grep".into(),
            json!({"pattern": "fn", "path": "src"}),
        ),
        (
            "fs.write".into(),
            json!({"path": "conformance.tmp", "content": "hello"}),
        ),
        (
            "edit.replace".into(),
            json!({"path": "src/main.rs", "old": "fn main", "new": "fn entry"}),
        ),
        ("git.status".into(), json!({})),
        ("git.diff".into(), json!({"staged": false})),
        ("shell.exec".into(), json!({"command": "echo conformance"})),
        ("context.manage".into(), json!({"op": "search"})),
        ("capability.manage".into(), json!({"op": "search"})),
    ]
}

/// Workspace-path calls that must be refused: absolute and parent-escaping
/// paths must never resolve into the workspace.
fn confined_calls() -> Vec<(String, Value)> {
    vec![
        ("fs.read".into(), json!({"path": "/etc/passwd"})),
        ("fs.list".into(), json!({"path": "/etc"})),
        (
            "search.grep".into(),
            json!({"pattern": "x", "path": "/etc"}),
        ),
        (
            "fs.write".into(),
            json!({"path": "/tmp/escape.txt", "content": "x"}),
        ),
        (
            "edit.replace".into(),
            json!({"path": "/etc/passwd", "old": "a", "new": "b"}),
        ),
        ("fs.read".into(), json!({"path": "../outside.txt"})),
        (
            "fs.write".into(),
            json!({"path": "src/../outside.txt", "content": "x"}),
        ),
    ]
}

#[tokio::test]
async fn builtin_catalog_passes_the_conformance_harness() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let dispatcher = BuiltinToolDispatcher::new(workspace.clone());
    let broker = WorkspaceOutputBroker::new(Arc::new(workspace));

    // 1. Schema contract for every known tool (catalog rows + meta specs).
    let mut report = check_catalog(&dispatcher).await;

    // 2. Output envelope for one representative execution per tool, with the
    //    trusted broker applied exactly as the kernel applies it.
    for (name, arguments) in representative_calls() {
        let outcome = dispatcher.execute(request(&name, arguments)).await;
        match outcome {
            Ok(outcome) => {
                let output = bound_value(outcome, &broker, RunId::new()).await;
                report.subjects_checked += 1;
                report.extend(check_output_envelope(&output));
            }
            Err(error) => {
                report.subjects_checked += 1;
                report.extend(check_error_envelope(&error));
            }
        }
    }

    // 3. Path confinement: absolute/parent-escaping workspace paths are
    //    refused (structured error) or produce an explicit failure output —
    //    they must never read or write outside the workspace.
    for (name, arguments) in confined_calls() {
        let outcome = dispatcher.execute(request(&name, arguments.clone())).await;
        match outcome {
            Ok(outcome) => {
                let output = bound_value(outcome, &broker, RunId::new()).await;
                report.subjects_checked += 1;
                if output.ok {
                    report.push(agent_conformance::ConformanceViolation::new(
                        format!("confinement:{name}"),
                        "confinement",
                        format!(
                            "an absolute/parent-escaping workspace path must not succeed, got ok=true: {arguments}"
                        ),
                    ));
                }
                report.extend(check_output_envelope(&output));
            }
            Err(error) => {
                report.subjects_checked += 1;
                report.extend(check_error_envelope(&error));
            }
        }
    }

    assert!(
        report.is_clean(),
        "builtin catalog failed conformance:\n{}",
        report.render()
    );
}

#[tokio::test]
async fn every_catalog_spec_has_a_bounded_schema() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let dispatcher = BuiltinToolDispatcher::new(workspace);

    let mut violations = Vec::new();
    for entry in dispatcher.catalog() {
        if let Some(spec) = dispatcher.inspect_tool(&entry.name) {
            violations.extend(check_schema_contract(&spec));
        }
    }
    for spec in dispatcher.specs() {
        if dispatcher.inspect_tool(&spec.name).is_none() {
            violations.extend(check_schema_contract(&spec));
        }
    }
    assert!(violations.is_empty(), "{violations:?}");
}

#[tokio::test]
async fn control_tools_execute_and_stay_within_the_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(dir.path()).await.unwrap();
    let dispatcher = BuiltinToolDispatcher::new(workspace.clone());
    let broker = WorkspaceOutputBroker::new(Arc::new(workspace));

    // capability.manage search/inspect/load/unload must all return bounded
    // plain values.
    for op in ["search", "inspect", "load", "unload"] {
        let arguments = match op {
            "search" => json!({"op": "search", "query": "fs"}),
            "inspect" => json!({"op": "inspect", "name": "fs.read"}),
            "load" => json!({"op": "load", "name": "git.status"}),
            "unload" => json!({"op": "unload", "name": "git.status"}),
            _ => unreachable!(),
        };
        let outcome = dispatcher
            .execute(request("capability.manage", arguments))
            .await
            .expect("capability.manage must execute");
        let output = bound_value(outcome, &broker, RunId::new()).await;
        let violations = check_output_envelope(&output);
        assert!(violations.is_empty(), "{op}: {violations:?}");
    }
}

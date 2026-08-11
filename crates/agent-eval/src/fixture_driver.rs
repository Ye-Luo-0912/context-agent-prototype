//! Deterministic fixture evaluation: run one coding fixture through the
//! real runtime with the real builtin tool surface, a scripted model, and
//! the fixture's hidden verification — proving the M15 harness end to end
//! (tool execution, prepared-effect commit, verification, cost accounting)
//! without a provider.
//!
//! This is the harness skeleton of M15: the live A/B/C/D run against a real
//! model replaces only the `ScriptedModel`; the workspace, tool surface,
//! verification and accounting stay.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextEngine, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, ToolCall, ToolDispatcher, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig};
use agent_runtime::{ApprovalModule, ContextModule, ModelModule, ModuleHost, RuntimeInstance};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;
use tokio::sync::broadcast;

use crate::{metrics, mock_model::ScriptedModel, workload};

/// One fixture run: whether the hidden verification passed and the
/// all-module cost accounting of the run.
#[derive(Debug, Clone)]
pub struct FixtureEval {
    pub fixture_id: &'static str,
    pub passed: bool,
    pub metrics: metrics::RunMetrics,
}

/// Approval policy for the harness: everything is allowed, so the effect
/// fence and the tool surface are the only gates under test.
struct AllowAllGate;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

/// The scripted edit each fixture requires, expressed as tool calls the
/// scripted model emits. These mirror the fixtures' `expected_edit` and are
/// kept deterministic so the harness run is repeatable.
fn scripted_steps(fixture_id: &str) -> Vec<ToolCall> {
    let call = |id: &str, name: &str, arguments: serde_json::Value| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    };
    match fixture_id {
        "fix_off_by_one" => vec![call(
            "c1",
            "edit.replace",
            json!({"path": "src/util.py", "old": "items[i + 1]", "new": "items[i]"}),
        )],
        "implement_stub" => vec![call(
            "c1",
            "edit.replace",
            json!({"path": "src/math.py", "old": "pass", "new": "return x * 2"}),
        )],
        "rename_symbol" => vec![call(
            "c1",
            "edit.replace",
            json!({"path": "src/app.py", "old": "old_name", "new": "new_name", "replace_all": true}),
        )],
        "add_test" => vec![call(
            "c1",
            "fs.write",
            json!({"path": "src/calc.py", "content": "def add(a, b):\n    return a + b\n\ndef test_add():\n    assert add(2, 3) == 5\n"}),
        )],
        other => panic!("no scripted steps for fixture '{other}'"),
    }
}

/// Run one fixture to completion against the real builtin tool surface with
/// a scripted model, then score it with the fixture's hidden verification.
pub async fn run_fixture(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
) -> anyhow::Result<FixtureEval> {
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let model: Arc<dyn ModelTransport> = Arc::new(ScriptedModel::new(
        scripted_steps(fixture.id),
        format!("{}: done", fixture.id),
    ));
    let approval: Arc<dyn ApprovalGate> = Arc::new(AllowAllGate);

    let workspace = agent_workspace::Workspace::open(workspace_root).await?;
    let tools: Arc<dyn ToolDispatcher> =
        Arc::new(tool_runtime::BuiltinToolDispatcher::with_config(
            workspace,
            tool_runtime::ToolLifecycleConfig {
                always_loaded: vec![
                    "fs.list".into(),
                    "fs.read".into(),
                    "fs.write".into(),
                    "edit.replace".into(),
                    "search.grep".into(),
                    "git.status".into(),
                    "git.diff".into(),
                    "shell.exec".into(),
                    agent_contracts::CONTEXT_MANAGE.into(),
                    agent_contracts::CAPABILITY_MANAGE.into(),
                ],
                ..tool_runtime::ToolLifecycleConfig::default()
            },
        ));

    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(context_engine)))?;
    host.add_module(Arc::new(ModelModule::new(model)))?;
    host.add_module(Arc::new(agent_runtime::ToolModule::new(tools)))?;
    host.add_module(Arc::new(ApprovalModule::new(approval)))?;
    host.start().await?;

    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        host.registry().context_service()?,
        host.registry().model_provider()?,
        host.registry().tool_provider()?,
        host.registry().approval_policy()?,
        host.registry().event_store()?,
    ));
    let runtime = RuntimeInstance::spawn(host, kernel);
    let mut events = runtime.handle().subscribe();
    runtime.start().await?;

    let mut collected: Vec<RuntimeEventEnvelope> = Vec::new();
    runtime
        .handle()
        .user_message(fixture.description.to_string())
        .await?;
    wait_for_turn(&mut events, &mut collected)
        .await
        .map_err(|reason| anyhow::anyhow!(reason))?;
    let passed = workload::fixture_passes(fixture, workspace_root);
    runtime.shutdown().await?;

    Ok(FixtureEval {
        fixture_id: fixture.id,
        passed,
        metrics: metrics::aggregate_metrics(&collected),
    })
}

/// Collect events until the current turn completes, then hand the whole
/// turn to the metrics aggregator.
async fn wait_for_turn(
    events: &mut broadcast::Receiver<RuntimeEventEnvelope>,
    collected: &mut Vec<RuntimeEventEnvelope>,
) -> Result<(), String> {
    loop {
        match tokio::time::timeout(Duration::from_secs(120), events.recv()).await {
            Err(_) => return Err("fixture turn timed out".into()),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err("event stream closed".into());
            }
            Ok(Ok(envelope)) => {
                collected.push(envelope.clone());
                match envelope.event {
                    RuntimeEvent::TurnCompleted => return Ok(()),
                    RuntimeEvent::TurnCommitFailed { message, .. } => {
                        return Err(format!("turn commit failed: {message}"));
                    }
                    RuntimeEvent::Error { message } => {
                        return Err(format!("runtime error: {message}"));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workload::FIXTURES;

    /// Every fixture must complete successfully through the real tool
    /// surface, and the cost accounting must record the scripted edit.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_fixture_passes_through_the_real_tool_surface() {
        for fixture in &FIXTURES {
            let dir = tempfile::tempdir().unwrap();
            workload::seed_fixture(fixture, dir.path());

            let eval = run_fixture(fixture, dir.path()).await.unwrap();

            assert!(
                eval.passed,
                "fixture '{}' must pass after the scripted edit",
                fixture.id
            );
            assert!(
                eval.metrics.tool_calls >= 1,
                "fixture '{}' must have driven at least one tool call, got {:?}",
                fixture.id,
                eval.metrics
            );
            assert!(
                eval.metrics.turns >= 1,
                "fixture '{}' must have run at least one turn",
                fixture.id
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fixture_run_records_the_effect_in_the_workspace() {
        // The scripted edit must actually land (the effect committed behind
        // the generation fence), so the fixture's hidden verification can
        // read the fixed file.
        let fixture = &FIXTURES[0];
        let dir = tempfile::tempdir().unwrap();
        workload::seed_fixture(fixture, dir.path());
        let eval = run_fixture(fixture, dir.path()).await.unwrap();
        assert!(eval.passed);
        let content = std::fs::read_to_string(dir.path().join("src/util.py")).unwrap_or_default();
        assert!(content.contains("items[i]"), "the edit must have landed");
    }
}

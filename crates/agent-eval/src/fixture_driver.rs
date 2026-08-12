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

/// The changed file each fixture works on, used by the multi-turn script.
fn fixture_file(fixture_id: &str) -> &'static str {
    match fixture_id {
        "fix_off_by_one" => "src/util.py",
        "implement_stub" => "src/math.py",
        "rename_symbol" => "src/app.py",
        "add_test" => "src/calc.py",
        other => panic!("no fixture file for '{other}'"),
    }
}

/// Multi-turn script for the cross-engine comparison: the fixture's edit,
/// then a re-read of the changed file and a confirmation — the extra turns
/// are where append-only accumulates history and the dynamic working set
/// does not, so the token difference is measurable.
fn multi_turn_steps(fixture_id: &str) -> Vec<ToolCall> {
    let call = |id: &str, name: &str, arguments: serde_json::Value| ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    };
    let mut steps = scripted_steps(fixture_id);
    // Two more turns each re-read the changed file: every re-read append
    // adds another observation to the append-only transcript while the
    // dynamic working set only keeps the current view.
    for round in 2..=3 {
        steps.push(call(
            &format!("c{round}"),
            "fs.read",
            json!({"path": fixture_file(fixture_id)}),
        ));
    }
    steps
}

/// User prompts for the multi-turn comparison: the task, then two re-read
/// requests and a summary request — five turns of accumulated history for
/// the append-only engine to carry, and only a bounded working set for the
/// dynamic engine.
pub fn multi_turn_prompts(fixture: &workload::CodingFixture) -> Vec<String> {
    vec![
        fixture.description.to_string(),
        "Now read the file again and confirm the change is in place.".to_string(),
        "Read the file once more and double-check that every reference is consistent.".to_string(),
        "Re-read the file and verify the final state against the task description.".to_string(),
        "Summarize the change you made and the verification you performed.".to_string(),
    ]
}

/// One engine's row in the cross-engine comparison.
#[derive(Debug, Clone)]
pub struct EngineRun {
    pub engine: &'static str,
    pub eval: FixtureEval,
}

/// Run one fixture through the append-only, rolling-summary and dynamic
/// engines on the same multi-turn script and compare the all-module cost.
/// Each engine gets a fresh scripted model instance (the script counter is
/// per-instance), so the only difference between the rows is the context
/// policy.
pub async fn compare_engines(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
) -> anyhow::Result<Vec<EngineRun>> {
    let prompts = multi_turn_prompts(fixture);
    let turns: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let mut runs = Vec::new();
    for (name, engine) in [
        (
            "append",
            Arc::new(context_baselines::AppendOnlyEngine::new()) as Arc<dyn ContextEngine>,
        ),
        (
            "rolling",
            Arc::new(context_baselines::RollingSummaryEngine::new()) as Arc<dyn ContextEngine>,
        ),
        (
            "dynamic",
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()))
                as Arc<dyn ContextEngine>,
        ),
    ] {
        let model: Arc<dyn ModelTransport> = Arc::new(ScriptedModel::new(
            multi_turn_steps(fixture.id),
            format!("{}: done", fixture.id),
        ));
        let eval = run_fixture_with_engine(fixture, workspace_root, model, engine, &turns).await?;
        runs.push(EngineRun { engine: name, eval });
    }
    Ok(runs)
}

/// Human-readable comparison table for the cross-engine fixture runs.
pub fn render_comparison(runs: &[EngineRun]) -> String {
    let mut out = String::new();
    out.push_str("fixture cross-engine comparison (same scripted model, same tool surface):\n");
    for run in runs {
        let metrics = &run.eval.metrics;
        out.push_str(&format!(
            "  {:8} passed={} model_in={:>7} model_out={:>5} schema_tokens={:>6} rounds={} turns={} tool_calls={} lifecycle={}\n\
               {:8}   selected_items={} active_tokens={} residency(resident/warm/cold/ext)={}/{}/{}/{}\n",
            run.engine,
            run.eval.passed,
            metrics.model_input_tokens,
            metrics.model_output_tokens,
            metrics.schema_tokens_total,
            metrics.rounds,
            metrics.turns,
            metrics.tool_calls,
            metrics.lifecycle_transitions,
            "",
            metrics.selected_items_total,
            metrics.active_tokens_total,
            metrics.final_resident_items,
            metrics.final_warm_items,
            metrics.final_cold_items,
            metrics.final_external_items,
        ));
    }
    out
}

/// Run one fixture to completion against the real builtin tool surface with
/// a scripted model, then score it with the fixture's hidden verification.
pub async fn run_fixture(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
) -> anyhow::Result<FixtureEval> {
    let model: Arc<dyn ModelTransport> = Arc::new(ScriptedModel::new(
        scripted_steps(fixture.id),
        format!("{}: done", fixture.id),
    ));
    run_fixture_with_model(fixture, workspace_root, model).await
}

/// The M15 live path: the same harness with a real model transport. The
/// model under test sees the fixture description and the real tool surface;
/// the workspace, verification and accounting are identical to the
/// deterministic run. Requires a provider that accepts tool calls.
pub async fn run_fixture_with_model(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<FixtureEval> {
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    run_fixture_with_engine(
        fixture,
        workspace_root,
        model,
        context_engine,
        &[fixture.description],
    )
    .await
}

/// The M15 comparison path: the same harness on a caller-supplied context
/// engine (append-only / rolling / dynamic), driven through one or more
/// user turns. Cross-engine token differences only appear across turns —
/// inside one turn the TurnFrame carries the tool protocol, so every engine
/// sees the same in-turn context. The fixture's hidden verification runs
/// after the last turn.
pub async fn run_fixture_with_engine(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    context_engine: Arc<dyn ContextEngine>,
    turns: &[&str],
) -> anyhow::Result<FixtureEval> {
    let approval: Arc<dyn ApprovalGate> = Arc::new(AllowAllGate);

    let workspace = agent_workspace::Workspace::open(workspace_root).await?;
    let tools: Arc<dyn ToolDispatcher> =
        Arc::new(tool_runtime::BuiltinToolDispatcher::with_config(
            workspace.clone(),
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

    // One shared composition (agent-compose): the host wiring, kernel
    // services and actor spawn are identical to the TUI/CLI roots; the
    // harness only differs in the pieces it hands in (engine, model,
    // approval, plain dispatcher, no journal/artifacts/output broker).
    let composed = agent_compose::compose(agent_compose::ComposeConfig {
        workspace,
        context_engine,
        model,
        approval,
        base_tools: tools,
        capability_aware: false,
        journal: None,
        artifact_store: None,
        output_broker: None,
    })
    .await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;

    let mut collected: Vec<RuntimeEventEnvelope> = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        composed.handle().user_message(turn.to_string()).await?;
        if let Err(reason) = wait_for_turn(&mut events, &mut collected).await {
            composed.shutdown().await?;
            return Err(anyhow::anyhow!("turn {} failed: {reason}", index + 1));
        }
    }
    let passed = workload::fixture_passes(fixture, workspace_root);
    composed.shutdown().await?;

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

    /// The M15 acceptance, as a deterministic CI proxy: on the same real
    /// tool surface and the same scripted model, the dynamic engine must
    /// finish the multi-turn fixture with the same success while feeding
    /// the model measurably fewer input tokens than append-only.
    #[tokio::test(flavor = "multi_thread")]
    async fn dynamic_engine_saves_input_tokens_against_append_on_the_fixture_surface() {
        for fixture in &FIXTURES {
            let dir = tempfile::tempdir().unwrap();
            workload::seed_fixture(fixture, dir.path());

            let runs = compare_engines(fixture, dir.path()).await.unwrap();
            assert_eq!(runs.len(), 3, "fixture '{}'", fixture.id);

            let append = runs.iter().find(|run| run.engine == "append").unwrap();
            let rolling = runs.iter().find(|run| run.engine == "rolling").unwrap();
            let dynamic = runs.iter().find(|run| run.engine == "dynamic").unwrap();

            // Success does not regress: every engine drives the same
            // scripted edit through the real tool surface and passes the
            // hidden check.
            for run in &runs {
                assert!(
                    run.eval.passed,
                    "engine '{}' must pass fixture '{}'",
                    run.engine, fixture.id
                );
            }
            // The materialization baseline is actually recorded: every
            // engine's event stream carries ContextPrepared with a
            // non-empty residency snapshot.
            for run in &runs {
                assert!(
                    run.eval.metrics.materialize_rounds >= 1,
                    "engine '{}' must record materialization rounds on '{}'",
                    run.engine,
                    fixture.id
                );
                assert!(
                    run.eval.metrics.final_total_items >= 1,
                    "engine '{}' must record a residency snapshot on '{}'",
                    run.engine,
                    fixture.id
                );
            }
            // The multi-turn script actually exercised the tool surface.
            assert!(
                dynamic.eval.metrics.tool_calls >= 3,
                "fixture '{}'",
                fixture.id
            );
            assert!(dynamic.eval.metrics.turns >= 5, "fixture '{}'", fixture.id);

            // The dynamic working set must cost less model input than
            // either baseline on the same workload. The gap is a
            // real-but-bounded fraction of the total: tool schemas and the
            // system prompt are a large per-round fixed cost (the same
            // phenomenon the live M15 measurement reported), so the
            // assertion is directional plus a noise floor, not a large
            // ratio.
            for baseline in [append, rolling] {
                assert!(
                    dynamic.eval.metrics.model_input_tokens
                        < baseline.eval.metrics.model_input_tokens,
                    "fixture '{}': dynamic model_in {} must be below {} {}",
                    fixture.id,
                    dynamic.eval.metrics.model_input_tokens,
                    baseline.engine,
                    baseline.eval.metrics.model_input_tokens
                );
                assert!(
                    baseline.eval.metrics.model_input_tokens
                        - dynamic.eval.metrics.model_input_tokens
                        >= 300,
                    "fixture '{}': expected a material saving over {}, got {}",
                    fixture.id,
                    baseline.engine,
                    baseline.eval.metrics.model_input_tokens
                        - dynamic.eval.metrics.model_input_tokens
                );
            }
        }
    }
}

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
use std::time::{Duration, Instant};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextEngine, ContextHints, ContextKind,
    ContextQuery, ModelTransport, RuntimeEvent, RuntimeEventEnvelope, ToolCall, ToolDispatcher,
    ToolSpec, tokens,
};
use agent_runtime::RuntimeHandle;
use context_baselines::{RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;
use tokio::sync::broadcast;

use crate::{bundle, harvest, metrics, mock_model::ScriptedModel, suite, workload};

/// 脚本化 fixture：事件之间最多等这么久（本机工具面，不该接近这个上限）。
const SCRIPTED_IDLE: Duration = Duration::from_secs(120);
/// 真模型一轮（含 reasoning）可能远长于脚本化路径。
const LIVE_IDLE: Duration = Duration::from_secs(300);
/// 真模型 tool-loop 上限：A/B/C 共用。12 只够 file-only smoke；SWE-bench
/// 探索-编辑-验证需要更高。不要给 C 比 A 更高的 cap。
const LIVE_MAX_MODEL_ROUNDS: u32 = 48;

/// Per-turn wait policy. Live runs use a longer idle and a round cap.
#[derive(Clone, Copy)]
struct TurnLimits {
    idle: Duration,
    max_model_rounds: Option<u32>,
}

const SCRIPTED_LIMITS: TurnLimits = TurnLimits {
    idle: SCRIPTED_IDLE,
    max_model_rounds: None,
};

const LIVE_LIMITS: TurnLimits = TurnLimits {
    idle: LIVE_IDLE,
    max_model_rounds: Some(LIVE_MAX_MODEL_ROUNDS),
};

/// CI 确定性压缩器：live 注入 `ModelBackedCompactor`。
pub struct ScriptedCompactor;

#[async_trait::async_trait]
impl agent_contracts::BoundedCompactor for ScriptedCompactor {
    async fn compact(
        &self,
        request: agent_contracts::CompactionRequest,
    ) -> agent_contracts::AgentResult<agent_contracts::CompactionOutput> {
        let word_count = request.source.split_whitespace().count();
        Ok(agent_contracts::CompactionOutput {
            text: format!(
                "[rolling summary of {} earlier messages, {word_count} folded words: the work below is current (scripted digest)]",
                request.folded_items
            ),
            input_tokens: 0,
            output_tokens: 0,
        })
    }
}

/// One fixture run: whether the hidden verification passed and the
/// all-module cost accounting of the run.
#[derive(Debug, Clone)]
pub struct FixtureEval {
    pub fixture_id: String,
    pub passed: bool,
    pub metrics: metrics::RunMetrics,
    /// Wall time of this harness cell (composition + turns + shutdown).
    pub wall_ms: u64,
    /// 回合超时 / round-cap / runtime 错误。有值时 `passed` 为 false，
    /// 事件仍写入证据包，细胞不从配对里消失。
    pub error: Option<String>,
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
        "recall_after_fix" => vec![
            call(
                "c1",
                "edit.replace",
                json!({"path": "src/util.py", "old": "items[i + 1]", "new": "items[i]"}),
            ),
            call(
                "c2",
                "fs.write",
                json!({"path": "src/scratch.md", "content": "The office coffee machine is a Breville. The staff kitchen code is 200.\n"}),
            ),
            call(
                "c3",
                "fs.write",
                json!({"path": "src/scratch.md", "content": "The office coffee machine is a Breville. The staff kitchen code is 200.\nThe spare HDMI cable is in drawer 3. Standups are at 09:30.\n"}),
            ),
            call(
                "c4",
                "fs.write",
                json!({"path": "src/scratch.md", "content": "The office coffee machine is a Breville. The staff kitchen code is 200.\nThe spare HDMI cable is in drawer 3. Standups are at 09:30.\nThe wifi guest password is listed on the fridge. The printer is in room 4B.\n"}),
            ),
            call(
                "c5",
                "fs.write",
                json!({"path": "src/main.py", "content": "from util import visit_all\nprint(visit_all([1, 2, 3]))\n"}),
            ),
        ],
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
    /// Tokens the context policy itself injected into the final model view
    /// (summary markers, derived facts) — separate from the user/tool
    /// content that the input-token gap measures.
    pub manager_tokens: u64,
}

/// Count the manager/derivation tokens an engine would feed the model:
/// rolling-summary markers, task summaries and derived facts, measured
/// from a fresh final materialization. This makes the context policy's own
/// cost visible separately from the user/tool content that drives each
/// engine's input-token gap.
async fn manager_token_cost(engine: &dyn ContextEngine) -> anyhow::Result<u64> {
    let materialized = engine
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 100_000,
            hints: ContextHints::default(),
        })
        .await?;
    let visible = materialized
        .items
        .iter()
        .filter(|item| {
            item.kind == ContextKind::Summary || item.source.as_deref() == Some("derived")
        })
        .map(|item| tokens::approx_tokens(&item.content) as u64)
        .sum::<u64>();
    Ok(visible
        .saturating_add(materialized.diagnostics.compaction_input_tokens)
        .saturating_add(materialized.diagnostics.compaction_output_tokens))
}

/// Run one fixture through the append-only, rolling-summary and dynamic
/// engines on the same multi-turn script and compare the all-module cost.
/// Each engine gets a **fresh seeded workspace** under `workspace_root/<engine>`
/// so a later arm cannot inherit an earlier arm's edit.
pub async fn compare_engines(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
) -> anyhow::Result<Vec<EngineRun>> {
    compare_engines_with_model(fixture, workspace_root, None, None).await
}

/// Live pairing: real model, `live_turns`, independent workspaces.
/// Rolling uses the shared model-backed bounded compactor; scripted digest
/// remains the CI arm.
pub async fn compare_engines_live(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    pair: Option<&bundle::PairSink>,
) -> anyhow::Result<Vec<EngineRun>> {
    compare_engines_with_model(fixture, workspace_root, Some(model), pair).await
}

/// 套件 live：独立 workspace、打乱臂序、可执行 hidden command。
pub async fn compare_suite_live(
    task: &suite::SuiteTask,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    pair: Option<&bundle::PairSink>,
) -> anyhow::Result<Vec<EngineRun>> {
    compare_suite_with_model(task, workspace_root, move || model.clone(), pair, true).await
}

async fn compare_suite_with_model(
    task: &suite::SuiteTask,
    workspace_root: &Path,
    model: impl Fn() -> Arc<dyn ModelTransport>,
    pair: Option<&bundle::PairSink>,
    live: bool,
) -> anyhow::Result<Vec<EngineRun>> {
    let prompts = suite::live_turns(task);
    let turns: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let limits = if live { LIVE_LIMITS } else { SCRIPTED_LIMITS };
    let repeat = pair.map(|p| p.repeat).unwrap_or(1);
    let order = if live {
        crate::analysis::arm_order(&task.id, repeat)
    } else {
        crate::analysis::SCRIPTED_ARM_ORDER
    };
    let mut runs = Vec::new();
    for name in order {
        let cell_model = model();
        let engine = named_engine(name, live.then_some(cell_model.clone()))?;
        let root = workspace_root.join(name);
        eprintln!("  engine {name}: seeding {}", task.id);
        let _ = std::io::Write::flush(&mut std::io::stderr());
        suite::materialize_live_workspace(task, &root)?;
        eprintln!("  engine {name}: starting");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let eval = run_suite_with_engine(
            task,
            &root,
            cell_model,
            engine.clone(),
            &turns,
            limits,
            name,
            pair,
        )
        .await?;
        eprintln!(
            "  engine {name}: passed={} wall_ms={} model_in={} error={:?}",
            eval.passed, eval.wall_ms, eval.metrics.model_input_tokens, eval.error
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let manager_tokens = manager_token_cost(engine.as_ref()).await?;
        runs.push(EngineRun {
            engine: name,
            eval,
            manager_tokens,
        });
    }
    if let Some(pair) = pair {
        bundle::write_pair(pair, &order)?;
    }
    Ok(runs)
}

async fn compare_engines_with_model(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    live_model: Option<Arc<dyn ModelTransport>>,
    pair: Option<&bundle::PairSink>,
) -> anyhow::Result<Vec<EngineRun>> {
    let live = live_model.is_some();
    let prompts = if live || workload::scripted_one_tool_per_turn(fixture) {
        workload::live_turns(fixture)
    } else {
        multi_turn_prompts(fixture)
    };
    let turns: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let limits = if live { LIVE_LIMITS } else { SCRIPTED_LIMITS };
    let repeat = pair.map(|p| p.repeat).unwrap_or(1);
    let order = if live {
        crate::analysis::arm_order(fixture.id, repeat)
    } else {
        crate::analysis::SCRIPTED_ARM_ORDER
    };
    let mut runs = Vec::new();
    for name in order {
        let model: Arc<dyn ModelTransport> = match &live_model {
            Some(live) => live.clone(),
            None => Arc::new(scripted_model_for(fixture, /*compare_arm*/ true)),
        };
        let engine = named_engine(name, live_model.clone())?;
        let root = workspace_root.join(name);
        std::fs::create_dir_all(&root)?;
        workload::seed_fixture(fixture, &root);
        eprintln!("  engine {name}: starting");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let eval = run_fixture_with_engine(
            fixture,
            &root,
            model,
            engine.clone(),
            &turns,
            limits,
            name,
            pair,
        )
        .await?;
        eprintln!(
            "  engine {name}: passed={} wall_ms={} model_in={} error={:?}",
            eval.passed, eval.wall_ms, eval.metrics.model_input_tokens, eval.error
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let manager_tokens = manager_token_cost(engine.as_ref()).await?;
        runs.push(EngineRun {
            engine: name,
            eval,
            manager_tokens,
        });
    }
    if let Some(pair) = pair {
        bundle::write_pair(pair, &order)?;
    }
    Ok(runs)
}

fn named_engine(
    name: &'static str,
    live_model: Option<Arc<dyn ModelTransport>>,
) -> anyhow::Result<Arc<dyn ContextEngine>> {
    Ok(match name {
        "append" => Arc::new(context_baselines::AppendOnlyEngine::new()) as Arc<dyn ContextEngine>,
        "rolling" => {
            // 默认 9000 token 阈值在五轮 fixture 上不会折叠，所以评测臂用
            // 200/100，从第四轮开始折叠。CI 用脚本化压缩器；live 用同一
            // 模型上的有界压缩器。
            let rolling = RollingSummaryEngine::with_config(RollingConfig {
                summary_threshold_tokens: 200,
                keep_most_recent_tokens: 100,
            });
            let rolling = match live_model.clone() {
                Some(model) => rolling
                    .with_compactor(Arc::new(agent_compose::ModelBackedCompactor::new(model))),
                None => rolling.with_compactor(Arc::new(ScriptedCompactor)),
            };
            Arc::new(rolling) as Arc<dyn ContextEngine>
        }
        "dynamic" => {
            let engine = SimpleContextEngine::new(SimpleContextConfig::default());
            let engine = match live_model {
                Some(model) => {
                    engine.with_compactor(Arc::new(agent_compose::ModelBackedCompactor::new(model)))
                }
                None => engine,
            };
            Arc::new(engine) as Arc<dyn ContextEngine>
        }
        other => anyhow::bail!("unknown engine {other}"),
    })
}

/// Human-readable comparison table for the cross-engine fixture runs.
pub fn render_comparison(runs: &[EngineRun]) -> String {
    render_comparison_header(
        runs,
        "fixture cross-engine comparison (same scripted model, same tool surface):",
    )
}

/// Live pairing table: real model, `live_turns`, independent workspaces.
pub fn render_live_comparison(runs: &[EngineRun]) -> String {
    render_comparison_header(
        runs,
        "fixture live comparison (real model, live_turns, independent workspaces):",
    )
}

fn render_comparison_header(runs: &[EngineRun], header: &str) -> String {
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    for run in runs {
        let metrics = &run.eval.metrics;
        out.push_str(&format!(
            "  {:8} passed={} wall_ms={:>7} model_in={:>7} model_out={:>5} schema_tokens={:>6} rounds={} turns={} tool_calls={} lifecycle={} manager_tokens={} error={}\n\
               {:8}   selected_items={} active_tokens={} residency(resident/warm/cold/ext)={}/{}/{}/{}\n\
               {:8}   resident_bytes final={} peak={}\n\
               {:8}   materialize(p50/p95)={}ms/{}ms store(w/r/recalled)={}/{}/{}\n\
               {:8}   retrieval search={}/{} empty={} recovered={}/{} access(search/inspect/fetch/ack)={}/{}/{}/{}\n",
            run.engine,
            run.eval.passed,
            run.eval.wall_ms,
            metrics.model_input_tokens,
            metrics.model_output_tokens,
            metrics.schema_tokens_total,
            metrics.rounds,
            metrics.turns,
            metrics.tool_calls,
            metrics.lifecycle_transitions,
            run.manager_tokens,
            run.eval.error.as_deref().unwrap_or("-"),
            "",
            metrics.selected_items_total,
            metrics.active_tokens_total,
            metrics.final_resident_items,
            metrics.final_warm_items,
            metrics.final_cold_items,
            metrics.final_external_items,
            "",
            metrics.final_resident_bytes,
            metrics.peak_resident_bytes,
            "",
            metrics.materialize_ms_p50,
            metrics.materialize_ms_p95,
            metrics.store_write_bytes_total,
            metrics.store_read_bytes_total,
            metrics.store_recalled_items_total,
            "",
            metrics.search_calls,
            metrics.search_hits,
            metrics.search_empty,
            metrics.recovered_items,
            metrics.forgotten_items,
            metrics.access_search_hits,
            metrics.access_inspects,
            metrics.access_fetches,
            metrics.access_consumption_acks,
        ));
    }
    out
}

/// 脚本化模型：原四题的 `--compare-arm` 仍把多步工具塞进第一轮 tool-loop；
/// `recall_after_fix` 一轮一工具，对齐 live 的五轮用户输入。
fn scripted_model_for(fixture: &workload::CodingFixture, compare_arm: bool) -> ScriptedModel {
    let steps = if compare_arm && !workload::scripted_one_tool_per_turn(fixture) {
        multi_turn_steps(fixture.id)
    } else {
        scripted_steps(fixture.id)
    };
    let model = ScriptedModel::new(steps, format!("{}: done", fixture.id));
    if workload::scripted_one_tool_per_turn(fixture) {
        model.one_tool_per_turn()
    } else {
        model
    }
}

/// Run one fixture to completion against the real builtin tool surface with
/// a scripted model, then score it with the fixture's hidden verification.
pub async fn run_fixture(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
) -> anyhow::Result<FixtureEval> {
    let model: Arc<dyn ModelTransport> = Arc::new(scripted_model_for(fixture, false));
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let prompts = workload::live_turns(fixture);
    let turns: Vec<&str> = prompts.iter().map(String::as_str).collect();
    run_fixture_with_engine(
        fixture,
        workspace_root,
        model,
        context_engine,
        &turns,
        SCRIPTED_LIMITS,
        "dynamic",
        None,
    )
    .await
}

/// The M15 live path: the same harness with a real model transport. The
/// model under test sees `live_turns` and the real tool surface;
/// the workspace, verification and accounting are identical to the
/// deterministic run. Requires a provider that accepts tool calls.
pub async fn run_fixture_with_model(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<FixtureEval> {
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let prompts = workload::live_turns(fixture);
    let turns: Vec<&str> = prompts.iter().map(String::as_str).collect();
    run_fixture_with_engine(
        fixture,
        workspace_root,
        model,
        context_engine,
        &turns,
        LIVE_LIMITS,
        "dynamic",
        None,
    )
    .await
}

/// The M15 comparison path: the same harness on a caller-supplied context
/// engine (append-only / rolling / dynamic), driven through one or more
/// user turns. Cross-engine token differences only appear across turns —
/// inside one turn the TurnFrame carries the tool protocol, so every engine
/// sees the same in-turn context. The fixture's hidden verification runs
/// after the last turn.
async fn run_fixture_with_engine(
    fixture: &workload::CodingFixture,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    context_engine: Arc<dyn ContextEngine>,
    turns: &[&str],
    limits: TurnLimits,
    engine: &'static str,
    pair: Option<&bundle::PairSink>,
) -> anyhow::Result<FixtureEval> {
    let session =
        run_workspace_session(workspace_root, model, context_engine, turns, limits).await?;
    let passed = session.error.is_none() && workload::fixture_passes(fixture, workspace_root);
    let eval = FixtureEval {
        fixture_id: fixture.id.to_string(),
        passed,
        metrics: metrics::aggregate_metrics(&session.events),
        wall_ms: session.wall_ms,
        error: session.error.clone(),
    };
    if let Some(pair) = pair {
        bundle::write_cell(
            &pair.cell_dir(engine),
            fixture,
            engine,
            pair,
            &session.events,
            &eval.metrics,
            eval.passed,
            eval.wall_ms,
            eval.error.as_deref(),
            workspace_root,
            session.lagged,
            session.deltas_omitted,
        )?;
    }
    Ok(eval)
}

async fn run_suite_with_engine(
    task: &suite::SuiteTask,
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    context_engine: Arc<dyn ContextEngine>,
    turns: &[&str],
    limits: TurnLimits,
    engine: &'static str,
    pair: Option<&bundle::PairSink>,
) -> anyhow::Result<FixtureEval> {
    let session =
        run_workspace_session(workspace_root, model, context_engine, turns, limits).await?;
    let run_tag = format!(
        "pilot-{}-{}-r{}",
        harvest::instance_id_from_suite_id(&task.id).unwrap_or(&task.id),
        engine,
        pair.map(|p| p.repeat).unwrap_or(1)
    );
    // round-cap / timeout 已经是 ITT 失败；再拉 SWE-bench 镜像只烧时间。
    let commands = if session.error.is_some() {
        vec![workload::HiddenCommandResult {
            argv: vec!["evaluate".into()],
            stderr: format!(
                "skipped swebench docker: {}",
                session.error.as_deref().unwrap_or("session error")
            ),
            passed: false,
            ..workload::HiddenCommandResult::default()
        }]
    } else {
        match evaluate_after_live(task, workspace_root, &run_tag) {
            Ok(commands) => commands,
            Err(error) => vec![workload::HiddenCommandResult {
                argv: vec!["evaluate".into()],
                stderr: error.to_string(),
                passed: false,
                ..workload::HiddenCommandResult::default()
            }],
        }
    };
    let passed = session.error.is_none() && suite::all_hidden_passed(&commands);
    let eval = FixtureEval {
        fixture_id: task.id.clone(),
        passed,
        metrics: metrics::aggregate_metrics(&session.events),
        wall_ms: session.wall_ms,
        error: session.error.clone(),
    };
    if let Some(pair) = pair {
        bundle::write_suite_cell(
            &pair.cell_dir(engine),
            task,
            engine,
            pair,
            &session.events,
            &eval.metrics,
            eval.passed,
            eval.wall_ms,
            eval.error.as_deref(),
            workspace_root,
            session.lagged,
            session.deltas_omitted,
            commands,
        )?;
    }
    Ok(eval)
}

fn evaluate_after_live(
    task: &suite::SuiteTask,
    workspace_root: &Path,
    run_tag: &str,
) -> anyhow::Result<Vec<workload::HiddenCommandResult>> {
    if task.runtime != harvest::RUNTIME {
        return suite::evaluate_suite_task(task, workspace_root);
    }
    if !harvest::docker_opt_in() {
        anyhow::bail!("set AGENT_EVAL_SWEBENCH_DOCKER=1 to score a model patch");
    }
    let instance = harvest::instance_id_from_suite_id(&task.id)
        .ok_or_else(|| anyhow::anyhow!("{} is not a swebench suite id", task.id))?;
    let patch = harvest::git_model_patch(workspace_root)?;
    Ok(vec![harvest::run_prediction_eval(
        instance, &patch, run_tag,
    )?])
}

struct WorkspaceSession {
    events: Vec<RuntimeEventEnvelope>,
    lagged: u64,
    deltas_omitted: u64,
    wall_ms: u64,
    error: Option<String>,
}

async fn run_workspace_session(
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    context_engine: Arc<dyn ContextEngine>,
    turns: &[&str],
    limits: TurnLimits,
) -> anyhow::Result<WorkspaceSession> {
    let started = Instant::now();
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
        max_tool_rounds: limits.max_model_rounds.map(|n| n as usize),
    })
    .await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;

    let mut capture = EventCapture::default();
    let mut error = None;
    for (index, turn) in turns.iter().enumerate() {
        composed.handle().user_message(turn.to_string()).await?;
        if let Err(reason) =
            wait_for_turn(&mut events, &mut capture, composed.handle(), limits).await
        {
            error = Some(format!("turn {} failed: {reason}", index + 1));
            break;
        }
    }
    let _ = composed.shutdown().await;
    Ok(WorkspaceSession {
        events: capture.events,
        lagged: capture.lagged,
        deltas_omitted: capture.deltas_omitted,
        wall_ms: started.elapsed().as_millis() as u64,
        error,
    })
}

#[derive(Default)]
struct EventCapture {
    events: Vec<RuntimeEventEnvelope>,
    lagged: u64,
    deltas_omitted: u64,
}

/// Collect events until the current turn completes, then hand the whole
/// turn to the metrics aggregator.
async fn wait_for_turn(
    events: &mut broadcast::Receiver<RuntimeEventEnvelope>,
    capture: &mut EventCapture,
    handle: &RuntimeHandle,
    limits: TurnLimits,
) -> Result<(), String> {
    let mut model_rounds = 0u32;
    let mut cancelled_for_cap = false;
    loop {
        match tokio::time::timeout(limits.idle, events.recv()).await {
            Err(_) => return Err("fixture turn timed out".into()),
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                capture.lagged = capture.lagged.saturating_add(skipped);
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err("event stream closed".into());
            }
            Ok(Ok(envelope)) => {
                if matches!(envelope.event, RuntimeEvent::ModelDelta { .. }) {
                    capture.deltas_omitted = capture.deltas_omitted.saturating_add(1);
                    continue;
                }
                capture.events.push(envelope.clone());
                match envelope.event {
                    RuntimeEvent::ModelStarted { .. } => {
                        model_rounds = model_rounds.saturating_add(1);
                        if let Some(max) = limits.max_model_rounds
                            && model_rounds > max
                            && !cancelled_for_cap
                        {
                            cancelled_for_cap = true;
                            let _ = handle.cancel_turn().await;
                        }
                    }
                    RuntimeEvent::TurnCompleted => {
                        if cancelled_for_cap {
                            return Err(format!(
                                "live model-round cap ({}) exceeded",
                                limits.max_model_rounds.unwrap_or(0)
                            ));
                        }
                        return Ok(());
                    }
                    RuntimeEvent::TurnCancelled { .. } => {
                        return Err(if cancelled_for_cap {
                            format!(
                                "live model-round cap ({}) exceeded",
                                limits.max_model_rounds.unwrap_or(0)
                            )
                        } else {
                            "turn cancelled".into()
                        });
                    }
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

    #[test]
    fn live_round_cap_is_shared_and_hosts_swebench() {
        assert_eq!(LIVE_MAX_MODEL_ROUNDS, 48);
        assert_eq!(LIVE_LIMITS.max_model_rounds, Some(48));
        assert_eq!(SCRIPTED_LIMITS.max_model_rounds, None);
    }

    #[tokio::test]
    async fn named_rolling_engine_uses_scripted_compactor_without_a_live_model() {
        use agent_contracts::{ContextIngress, ContextMaintenanceTrigger};

        let engine = named_engine("rolling", None).unwrap();
        for turn in 0..20 {
            engine
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn} enough tokens to fold the rolling window"),
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::UserInput)
                .await
                .unwrap();
        }
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let summary = materialized
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Summary)
            .expect("rolling CI arm must fold");
        assert!(
            summary.content.contains("scripted digest"),
            "CI rolling must use ScriptedCompactor, got: {}",
            summary.content
        );
        assert_eq!(
            materialized.diagnostics.compaction_input_tokens, 0,
            "scripted compact reports no provider tokens"
        );
    }

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

    /// The manager/derivation token counter is engine-specific: append-only
    /// injects nothing, the rolling arm injects its summary marker once it
    /// folds, and the dynamic engine injects none without a task completion
    /// or a derive.
    #[tokio::test]
    async fn manager_token_cost_is_engine_specific() {
        use agent_contracts::{ContextIngress, ContextMaintenanceTrigger};

        let append = context_baselines::AppendOnlyEngine::new();
        assert_eq!(manager_token_cost(&append).await.unwrap(), 0);

        let rolling = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 20,
            keep_most_recent_tokens: 5,
        })
        .with_compactor(Arc::new(ScriptedCompactor));
        for turn in 0..20 {
            rolling
                .ingest(ContextIngress::UserMessage {
                    content: format!("turn {turn}"),
                })
                .await
                .unwrap();
            rolling
                .maintain(ContextMaintenanceTrigger::UserInput)
                .await
                .unwrap();
        }
        assert!(
            manager_token_cost(&rolling).await.unwrap() > 0,
            "the folded summary marker must count as manager tokens"
        );

        let dynamic = SimpleContextEngine::new(SimpleContextConfig::default());
        assert_eq!(manager_token_cost(&dynamic).await.unwrap(), 0);
    }

    /// The M15 acceptance, as a deterministic CI proxy: on the same real
    /// tool surface and the same scripted model, the dynamic engine must
    /// finish the multi-turn fixture with the same success while feeding
    /// the model measurably fewer input tokens than append-only.
    #[tokio::test(flavor = "multi_thread")]
    async fn dynamic_engine_saves_input_tokens_against_append_on_the_fixture_surface() {
        for fixture in &FIXTURES {
            // 多轮回忆题走 live_turns，不套原四题的「五轮重读 + ≥300 token」断言。
            if workload::scripted_one_tool_per_turn(fixture) {
                continue;
            }
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
                assert!(
                    run.eval.metrics.peak_resident_bytes > 0,
                    "engine '{}' must record Resident bytes on '{}'",
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
            // append-only on the same workload. The gap is a
            // real-but-bounded fraction of the total: tool schemas and the
            // system prompt are a large per-round fixed cost (the same
            // phenomenon the live M15 measurement reported), so the
            // assertion is directional plus a noise floor, not a large
            // ratio.
            assert!(
                dynamic.eval.metrics.model_input_tokens < append.eval.metrics.model_input_tokens,
                "fixture '{}': dynamic model_in {} must be below append {}",
                fixture.id,
                dynamic.eval.metrics.model_input_tokens,
                append.eval.metrics.model_input_tokens
            );
            assert!(
                append.eval.metrics.model_input_tokens - dynamic.eval.metrics.model_input_tokens
                    >= 300,
                "fixture '{}': expected a material saving over append, got {}",
                fixture.id,
                append.eval.metrics.model_input_tokens - dynamic.eval.metrics.model_input_tokens
            );

            // The rolling arm is a *real* rolling baseline on the fixture
            // workload: the thresholds fold the history, the scripted
            // summarizer's marker is present in the final materialization
            // (its cost is counted as manager tokens), and folding never
            // makes the arm cost more than append-only.
            assert!(
                rolling.manager_tokens > 0,
                "fixture '{}': the rolling arm must fold and count its summary marker, got manager_tokens={}",
                fixture.id,
                rolling.manager_tokens
            );
            assert!(
                rolling.eval.metrics.model_input_tokens <= append.eval.metrics.model_input_tokens,
                "fixture '{}': rolling model_in {} must not exceed append {}",
                fixture.id,
                rolling.eval.metrics.model_input_tokens,
                append.eval.metrics.model_input_tokens
            );
        }
    }

    /// Later engines must not inherit an earlier engine's edit: each arm
    /// seeds its own subdirectory.
    #[tokio::test(flavor = "multi_thread")]
    async fn compare_engines_seeds_an_independent_workspace_per_engine() {
        let fixture = &FIXTURES[0];
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/util.py"), "already broken parent\n").unwrap();

        let runs = compare_engines(fixture, dir.path()).await.unwrap();
        for run in &runs {
            assert!(
                run.eval.passed,
                "engine '{}' must pass from its own seed, not the poisoned parent",
                run.engine
            );
        }
        let parent = std::fs::read_to_string(dir.path().join("src/util.py")).unwrap();
        assert!(
            parent.contains("already broken parent"),
            "the parent workspace must stay untouched"
        );
    }

    /// `recall_after_fix`：五轮用户输入、一轮一工具，三引擎各自独立 workspace 都过 hidden check。
    #[tokio::test(flavor = "multi_thread")]
    async fn recall_after_fix_passes_on_all_engines_with_independent_workspaces() {
        let fixture = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "recall_after_fix")
            .unwrap();
        assert_eq!(
            scripted_steps(fixture.id).len(),
            workload::live_turns(fixture).len()
        );
        let dir = tempfile::tempdir().unwrap();
        let runs = compare_engines(fixture, dir.path()).await.unwrap();
        assert_eq!(runs.len(), 3);
        for run in &runs {
            assert!(
                run.eval.passed,
                "engine '{}' must pass recall_after_fix",
                run.engine
            );
            assert!(
                run.eval.metrics.turns >= 5,
                "engine '{}' must run the five live turns, got {}",
                run.engine,
                run.eval.metrics.turns
            );
            assert!(
                run.eval.metrics.tool_calls >= 5,
                "engine '{}' must emit one tool per live turn, got {}",
                run.engine,
                run.eval.metrics.tool_calls
            );
            let util = std::fs::read_to_string(dir.path().join(run.engine).join("src/util.py"))
                .unwrap_or_default();
            assert!(
                !util.contains("i + 1"),
                "engine '{}' must keep the util fix",
                run.engine
            );
            let main = std::fs::read_to_string(dir.path().join(run.engine).join("src/main.py"))
                .unwrap_or_default();
            assert!(
                main.contains("visit_all"),
                "engine '{}' must write main.py",
                run.engine
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_engines_writes_a_rebuildable_evidence_pair() {
        let fixture = &FIXTURES[0];
        let dir = tempfile::tempdir().unwrap();
        let pair = bundle::PairSink {
            root: dir.path().join("evidence"),
            fixture_id: fixture.id.to_string(),
            repeat: 1,
            repeats: 1,
            live: false,
        };
        let runs = compare_engines_with_model(fixture, dir.path(), None, Some(&pair))
            .await
            .unwrap();
        assert!(
            runs.iter()
                .all(|run| run.eval.passed && run.eval.error.is_none())
        );
        let pair_dir = pair.root.join(fixture.id).join("r1");
        assert!(pair_dir.join("pair.json").is_file());
        for engine in ["append", "rolling", "dynamic"] {
            let cell = pair_dir.join(engine);
            assert!(cell.join("events.jsonl").is_file(), "{engine}");
            assert!(cell.join("summary.json").is_file(), "{engine}");
            let summary: bundle::CellSummary =
                serde_json::from_str(&std::fs::read_to_string(cell.join("summary.json")).unwrap())
                    .unwrap();
            assert!(summary.seq_contiguous, "{engine}");
            assert!(summary.passed, "{engine}");
        }
        let pair_doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(pair_dir.join("pair.json")).unwrap())
                .unwrap();
        assert_eq!(
            pair_doc["cells"][0]["dir"].as_str(),
            Some("append"),
            "pair.json must store portable relative cell dirs"
        );
        assert_eq!(
            pair_doc["analysis_schema"].as_str(),
            Some(bundle::ANALYSIS_SCHEMA)
        );
        assert_eq!(
            pair_doc["arm_order"].as_array().map(|rows| rows
                .iter()
                .filter_map(|row| row.as_str())
                .collect::<Vec<_>>()),
            Some(vec!["append", "rolling", "dynamic"])
        );
        let shown = bundle::render_evidence(&pair_dir).unwrap();
        assert!(
            shown.contains("append") && shown.contains("dynamic"),
            "{shown}"
        );
    }

    fn oracle_model(task: &suite::SuiteTask) -> ScriptedModel {
        let steps = task
            .expected_files
            .iter()
            .enumerate()
            .map(|(index, file)| ToolCall {
                id: format!("c{}", index + 1),
                name: "fs.write".into(),
                arguments: json!({
                    "path": file.path,
                    "content": file.content,
                }),
            })
            .collect();
        ScriptedModel::new(steps, format!("{}: done", task.id))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suite_oracle_passes_hidden_commands_on_a_file_task() {
        let pack = suite::load_pack().unwrap();
        let task = pack
            .tasks
            .iter()
            .find(|task| task.id == "python-itertools-batched")
            .expect("file-harvested task");
        assert!(!task.expected_files.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let runs = compare_suite_with_model(
            task,
            dir.path(),
            || Arc::new(oracle_model(task)),
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(runs.len(), 3);
        for run in &runs {
            assert!(
                run.eval.passed,
                "engine '{}' must pass {} after writing expected files: {:?}",
                run.engine, task.id, run.eval.error
            );
        }
    }
}

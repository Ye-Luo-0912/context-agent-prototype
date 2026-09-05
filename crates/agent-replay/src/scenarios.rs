//! Synthetic A/B/C experiment scenarios.
//!
//! Each scenario is a deterministic scripted `RuntimeEventEnvelope` sequence
//! mirroring the kernel's event pattern (user message -> maintain -> model
//! rounds with tool results -> assistant reply -> maintain -> full GC ->
//! TurnCompleted). Full GC is required: without it C only archives
//! attention and the Resident heap stays as large as append-only. The
//! replay harness compares token cost, over-budget turns and context churn.

use std::sync::Arc;

use agent_contracts::{
    ContextDiagnostics, ContextEngine, ContextGcReport, ContextMaintenanceReport,
    ContextMaintenanceTrigger, RunId, RuntimeEvent, RuntimeEventEnvelope, TaskId, ToolOutput,
};
use context_baselines::{AppendOnlyEngine, RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

use crate::{ReplayConfig, ReplayOutcome, run_engine};

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    pub events: Vec<RuntimeEventEnvelope>,
}

/// The seven comparison scenarios.
pub fn all_scenarios() -> Vec<Scenario> {
    vec![
        long_refactor(),
        test_fix_loop(),
        task_switch_and_return(),
        superseded_decisions(),
        high_volume_irrelevant_output(),
        completed_then_unrelated(),
        pinned_constraint(),
    ]
}

/// The three context policies under comparison.
pub fn engine_variants() -> Vec<(&'static str, Arc<dyn ContextEngine>)> {
    vec![
        ("A append-only", Arc::new(AppendOnlyEngine::new())),
        (
            "B rolling-summary",
            Arc::new(RollingSummaryEngine::with_config(RollingConfig::default())),
        ),
        (
            "C dynamic",
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        ),
    ]
}

/// Configuration for the A/B/C comparison: a small context window (12 K
/// tokens) so the cost of unbounded growth shows up quickly.
pub fn compare_config() -> ReplayConfig {
    ReplayConfig {
        budget_tokens: 12_000,
        ..ReplayConfig::default()
    }
}

/// Replay one scenario through all three engines and collect outcomes.
pub async fn compare_scenario(
    scenario: &Scenario,
    config: &ReplayConfig,
) -> anyhow::Result<Vec<(&'static str, ReplayOutcome)>> {
    let mut results = Vec::new();
    for (label, engine) in engine_variants() {
        let outcome = run_engine(engine, &scenario.events, config).await?;
        results.push((label, outcome));
    }
    Ok(results)
}

/// Human-readable A/B/C comparison table for one scenario.
pub fn render_comparison(scenario: &Scenario, results: &[(&str, ReplayOutcome)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "scenario: {} - {}\n",
        scenario.name, scenario.description
    ));
    out.push_str(&format!(
        "  {:18} {:>13} {:>12} {:>11} {:>6} {:>10} {:>9} {:>10} {:>10}\n",
        "engine",
        "in_tok_total",
        "in_tok_max",
        "over_budget",
        "churn",
        "final_total",
        "final_active",
        "res_bytes",
        "peak_bytes"
    ));
    for (label, outcome) in results {
        out.push_str(&format!(
            "  {:18} {:>13} {:>12} {:>11} {:>6} {:>10} {:>9} {:>10} {:>10}\n",
            label,
            outcome.input_tokens_total,
            outcome.input_tokens_max,
            outcome.over_budget_snapshots,
            outcome.transitions_total,
            outcome.final_diagnostics.total_items,
            outcome.final_diagnostics.active_items,
            outcome.final_diagnostics.resident_bytes,
            outcome.peak_resident_bytes,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Scenario building blocks
// ---------------------------------------------------------------------------

fn dummy_report() -> ContextMaintenanceReport {
    ContextMaintenanceReport::default()
}

struct Script {
    run: RunId,
    seq: u64,
    events: Vec<RuntimeEventEnvelope>,
    /// Stable task identity per goal, mirroring the runtime's TaskManager:
    /// re-focusing a goal resumes the same task instead of minting a new
    /// one, so replay exercises the real suspension/resume semantics.
    tasks: std::collections::HashMap<String, TaskId>,
    /// The most recently focused task; `done` completes it so the engine
    /// archives the right working set.
    current_task: Option<TaskId>,
}

impl Script {
    fn new() -> Self {
        Self {
            run: RunId::new(),
            seq: 0,
            events: Vec::new(),
            tasks: std::collections::HashMap::new(),
            current_task: None,
        }
    }

    fn push(&mut self, event: RuntimeEvent) {
        self.seq += 1;
        self.events.push(RuntimeEventEnvelope {
            run_id: self.run,
            seq: self.seq,
            timestamp_ms: self.seq,
            event,
        });
    }

    /// A `ContextPrepared` precedes every model round (matches the kernel).
    fn prepare(&mut self) {
        self.push(RuntimeEvent::ContextPrepared {
            diagnostics: ContextDiagnostics::default(),
            selected: Vec::new(),
            materialize_ms: 0,
        });
    }

    /// One user turn: user message, tool rounds, then a final assistant reply.
    fn turn(&mut self, user: &str, tools: &[ToolOutput]) {
        self.push(RuntimeEvent::user_message_accepted(user));
        self.push(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::UserInput,
            report: dummy_report(),
        });
        for output in tools {
            self.prepare();
            self.push(RuntimeEvent::ToolFinished {
                output: output.clone(),
                facts: None,
            });
            self.push(RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::AfterTool,
                report: dummy_report(),
            });
        }
        self.prepare();
        self.push(RuntimeEvent::AssistantMessage {
            content: "ok".into(),
        });
        self.push(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::AfterModel,
            report: dummy_report(),
        });
        // 对齐 RuntimeActor 回合提交：AfterModel 维护之后、TurnCompleted
        // 之前跑一次 full GC。不发这条事件时 C 只归档注意力，Resident 堆
        // 会和 append-only 一样大。
        self.push(RuntimeEvent::ContextGc {
            report: ContextGcReport::default(),
        });
        self.push(RuntimeEvent::TurnCompleted);
    }

    fn focus(&mut self, goal: &str) {
        let task_id = self.tasks.entry(goal.to_string()).or_default().to_owned();
        self.current_task = Some(task_id);
        self.push(RuntimeEvent::FocusChanged {
            task_id,
            goal: goal.into(),
        });
        self.push(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::FocusChanged,
            report: dummy_report(),
        });
    }

    fn pin(&mut self, content: &str) {
        self.push(RuntimeEvent::Pinned {
            content: content.into(),
        });
        self.push(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::FocusChanged,
            report: dummy_report(),
        });
    }

    fn done(&mut self, summary: &str) {
        let task_id = self.current_task.take().unwrap_or_default();
        self.push(RuntimeEvent::TaskCompleted {
            task_id,
            anchor_revision: 0,
            summary: summary.into(),
        });
        self.push(RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::TaskCompleted,
            report: dummy_report(),
        });
        // 对齐完成后的 compact：已完成任务的工作集离开 Resident 堆。
        self.push(RuntimeEvent::ContextGc {
            report: ContextGcReport::default(),
        });
    }

    fn finish(mut self, name: &'static str, description: &'static str) -> Scenario {
        self.push(RuntimeEvent::RunCompleted);
        Scenario {
            name,
            description,
            events: self.events,
        }
    }
}

fn tool(name: &str, ok: bool, content: &str) -> ToolOutput {
    ToolOutput {
        call_id: "call".into(),
        tool_name: name.into(),
        ok,
        summary: format!("{name}: {} chars", content.chars().count()),
        model_content: content.into(),
        artifact_ref: Some(format!("artifact://run/{name}.log")),
        metadata: json!({}),
    }
}

fn read_snippet(path: &str, body: &str) -> ToolOutput {
    let mut output = tool("fs.read", true, &format!("{path}:\n{body}"));
    output.metadata = json!({
        "path": path,
        "revision": "replay",
    });
    output
}

/// Deterministic "build log" of `lines` lines, ~90 chars each, so baseline A
/// accumulates tens of thousands of tokens on heavy scenarios.
fn big_log(prefix: &str, lines: usize) -> String {
    let sentence = "worker-42 INFO module=core::auth took 137ms tests=12 passed=12 failed=0 memory=88MB heap=64MB gc=3 cycles=2;";
    (0..lines)
        .map(|i| format!("{prefix}[{i:04}] {sentence}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn test_log(failing: bool, lines: usize) -> ToolOutput {
    let body = big_log(if failing { "FAIL " } else { "PASS " }, lines);
    // The green round runs through the trusted verification recipe: under
    // the error-verification contract only a verify.run success proves an
    // error fixed.
    tool("verify.run", !failing, &body)
}

// ---------------------------------------------------------------------------
// The seven scenarios
// ---------------------------------------------------------------------------

/// 1. A long refactor over a few changing files.
fn long_refactor() -> Scenario {
    let mut script = Script::new();
    script.focus("Refactor AuthService authentication flow to async");
    for step in 0..24 {
        let file = if step % 3 == 0 {
            "src/auth/login.rs"
        } else if step % 3 == 1 {
            "src/auth/session.rs"
        } else {
            "src/auth/token.rs"
        };
        let snippet = format!(
            "fn handle_{step}() {{\n    let user = authenticate(request, &db).await?;\n    let session = Session::start(user, &db).await?;\n    Ok(session)\n}}\n"
        );
        let tools = vec![read_snippet(file, &snippet), test_log(step < 22, 22)];
        script.turn(&format!("step {step}: refactor {file} to async"), &tools);
    }
    script.finish(
        "long_refactor",
        "24-turn async refactor across three changing files with test runs",
    )
}

/// 2. Repeated test/fix loops with large logs.
fn test_fix_loop() -> Scenario {
    let mut script = Script::new();
    script.focus("Make the integration test suite green");
    for round in 0..16 {
        let failing = round < 15;
        let tools = vec![
            read_snippet(
                "tests/integration.rs",
                &format!("// round {round}: assertions around auth flows\n"),
            ),
            test_log(failing, 34),
        ];
        script.turn(
            &format!("round {round}: run tests and fix the next failure"),
            &tools,
        );
    }
    script.finish(
        "test_fix_loop",
        "16 test/fix rounds with 34-line build logs, green at the end",
    )
}

/// 3. Explicit task switch and return.
fn task_switch_and_return() -> Scenario {
    let mut script = Script::new();
    script.focus("task A: refactor auth to async");
    for turn in 0..12 {
        script.turn(
            &format!("auth step {turn}: move login to async"),
            &[read_snippet(
                "src/auth/login.rs",
                "async fn login() { ... }\n",
            )],
        );
    }
    script.focus("task B: add request logging");
    for turn in 0..10 {
        script.turn(
            &format!("logging step {turn}: add middleware"),
            &[read_snippet(
                "src/http/middleware.rs",
                "fn log_request() { ... }\n",
            )],
        );
    }
    script.focus("task A: finish the auth refactor");
    for turn in 0..6 {
        script.turn(
            &format!("auth step {turn}: finish session handling"),
            &[read_snippet(
                "src/auth/session.rs",
                "async fn refresh() { ... }\n",
            )],
        );
    }
    script.finish(
        "task_switch_and_return",
        "task A (12) -> task B (10) -> back to task A (6)",
    )
}

/// 4. Contradictory / superseded design decisions.
fn superseded_decisions() -> Scenario {
    let mut script = Script::new();
    script.focus("Implement configuration loading");
    let decisions = [
        "use TOML for config",
        "actually switch to YAML, TOML is too verbose",
        "no, JSON is the standard here, drop YAML",
        "revert to TOML after all, the CLI tooling supports it",
        "add env var overrides on top of TOML",
        "drop TOML sections, flat keys only",
    ];
    for (index, decision) in decisions.iter().enumerate() {
        script.turn(&format!("decision {index}: {decision}"), &[]);
    }
    for step in 6..16 {
        script.turn(
            &format!("step {step}: wire config loading (current: flat TOML keys)"),
            &[read_snippet(
                "src/config.rs",
                "fn load() -> Config { /* per latest decision */ }\n",
            )],
        );
    }
    script.finish(
        "superseded_decisions",
        "6 superseding design decisions then 10 turns of implementation",
    )
}

/// 5. High-volume irrelevant tool output.
fn high_volume_irrelevant_output() -> Scenario {
    let mut script = Script::new();
    script.focus("Add CSV export to the report endpoint");
    for turn in 0..16 {
        script.turn(
            &format!("step {turn}: continue CSV export"),
            &[test_log(false, 60)],
        );
    }
    script.finish(
        "high_volume_irrelevant_output",
        "16 turns each dumping a 60-line build log unrelated to the task",
    )
}

/// 6. Completed task followed by an unrelated task.
fn completed_then_unrelated() -> Scenario {
    let mut script = Script::new();
    script.focus("task 1: fix pagination bug in /items");
    for turn in 0..10 {
        script.turn(
            &format!("pagination step {turn}: investigate and patch"),
            &[
                read_snippet("src/api/items.rs", "fn list() { ... }\n"),
                test_log(turn < 9, 12),
            ],
        );
    }
    script.done("pagination bug fixed: off-by-one in cursor decode");
    script.focus("task 2: add CSV export endpoint");
    for turn in 0..10 {
        script.turn(
            &format!("csv step {turn}: implement export"),
            &[read_snippet("src/api/export.rs", "fn export() { ... }\n")],
        );
    }
    script.finish(
        "completed_then_unrelated",
        "completed pagination task (10 turns) then unrelated CSV task (10 turns)",
    )
}

/// 7. A pinned constraint across many turns and tasks.
fn pinned_constraint() -> Scenario {
    let mut script = Script::new();
    script.pin("Never edit files under generated/ or vendor/");
    script.focus("task 1: fix the build");
    for turn in 0..5 {
        script.turn(
            &format!("build step {turn}"),
            &[read_snippet("src/main.rs", "fn main() {}\n")],
        );
    }
    script.focus("task 2: add telemetry");
    for turn in 0..5 {
        script.turn(
            &format!("telemetry step {turn}"),
            &[read_snippet("src/telemetry.rs", "fn emit() {}\n")],
        );
    }
    script.focus("task 3: harden error handling");
    for turn in 0..5 {
        script.turn(
            &format!("errors step {turn}"),
            &[read_snippet("src/errors.rs", "fn report() {}\n")],
        );
    }
    script.finish(
        "pinned_constraint",
        "one pinned constraint must survive 15 turns across three tasks",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_seven_scenarios_exist_and_are_non_trivial() {
        let scenarios = all_scenarios();
        assert_eq!(scenarios.len(), 7);
        for scenario in &scenarios {
            let turns = scenario
                .events
                .iter()
                .filter(|e| match &e.event {
                    RuntimeEvent::UserMessageAccepted { input } => input.is_applied(),
                    _ => false,
                })
                .count();
            assert!(
                turns >= 6,
                "{} should have many turns, got {turns}",
                scenario.name
            );
        }
    }

    #[test]
    fn scenario_turns_emit_full_gc_before_turn_completed() {
        for scenario in all_scenarios() {
            let mut last_was_gc = false;
            let mut turn_completed = 0usize;
            for envelope in &scenario.events {
                match &envelope.event {
                    RuntimeEvent::TurnCompleted => {
                        assert!(
                            last_was_gc,
                            "{}: TurnCompleted must follow ContextGc (runtime turn commit)",
                            scenario.name
                        );
                        turn_completed += 1;
                        last_was_gc = false;
                    }
                    RuntimeEvent::ContextGc { .. } => last_was_gc = true,
                    _ => last_was_gc = false,
                }
            }
            assert!(
                turn_completed >= 6,
                "{} should complete many turns, got {turn_completed}",
                scenario.name
            );
        }
    }

    #[tokio::test]
    async fn all_scenarios_replay_cleanly_through_all_engines() {
        let config = compare_config();
        for scenario in all_scenarios() {
            let results = compare_scenario(&scenario, &config).await.unwrap();
            assert_eq!(results.len(), 3);
            for (label, outcome) in &results {
                assert!(
                    outcome.input_tokens_total > 0,
                    "{}: {label} measured no input tokens",
                    scenario.name
                );
            }
        }
    }

    #[tokio::test]
    async fn dynamic_context_costs_far_less_on_heavy_scenarios() {
        let config = compare_config();
        for name in [
            "long_refactor",
            "test_fix_loop",
            "high_volume_irrelevant_output",
        ] {
            let scenario = all_scenarios()
                .into_iter()
                .find(|s| s.name == name)
                .expect(name);
            let results = compare_scenario(&scenario, &config).await.unwrap();
            let by_label = |label: &str| {
                results
                    .iter()
                    .find(|(l, _)| *l == label)
                    .map(|(_, o)| o)
                    .expect(label)
            };
            let append = by_label("A append-only");
            let dynamic = by_label("C dynamic");

            // Baseline A blows past the budget; the dynamic working set stays
            // far below it and costs a fraction of the input tokens.
            assert!(
                append.over_budget_snapshots > 0,
                "{name}: append-only should exceed the budget at some point"
            );
            assert_eq!(
                dynamic.over_budget_snapshots, 0,
                "{name}: dynamic working set should never exceed the budget"
            );
            assert!(
                dynamic.input_tokens_total < append.input_tokens_total * 3 / 5,
                "{name}: dynamic input {} should be well below append-only {}",
                dynamic.input_tokens_total,
                append.input_tokens_total
            );
            // The ephemeral TTL now counts user turns (an event burst must
            // not age it), so a single turn's flood of irrelevant output is
            // compressed by consumed-archive + generational eviction rather
            // than TTL — the saving is ~55% instead of >50% at the worst
            // scenario, still far below the append-only baseline.
            assert!(
                dynamic.input_tokens_max < append.input_tokens_max * 3 / 5,
                "{name}: dynamic peak {} should be well below append-only peak {}",
                dynamic.input_tokens_max,
                append.input_tokens_max
            );
            // Replay 必须跑回合边界 full GC，否则 C 只归档注意力、Resident
            // 堆和 A 一样大。堆字节（不是 prompt token）必须明显低于 A。
            assert!(
                dynamic.final_diagnostics.resident_bytes
                    < append.final_diagnostics.resident_bytes / 2,
                "{name}: dynamic resident_bytes {} must be below half of append-only {}",
                dynamic.final_diagnostics.resident_bytes,
                append.final_diagnostics.resident_bytes
            );
            assert!(
                dynamic.peak_resident_bytes < append.peak_resident_bytes / 2,
                "{name}: dynamic peak_resident_bytes {} must be below half of append-only {}",
                dynamic.peak_resident_bytes,
                append.peak_resident_bytes
            );
        }
    }

    #[tokio::test]
    async fn completed_task_detail_is_archived_not_resurrected() {
        // On the completed_then_unrelated scenario, the dynamic engine should
        // archive task-1 detail after TaskCompleted; the append-only engine
        // keeps every message active (contamination risk).
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "completed_then_unrelated")
            .expect("scenario");
        let results = compare_scenario(&scenario, &config).await.unwrap();
        let append = results
            .iter()
            .find(|(l, _)| *l == "A append-only")
            .map(|(_, o)| o)
            .unwrap();
        let dynamic = results
            .iter()
            .find(|(l, _)| *l == "C dynamic")
            .map(|(_, o)| o)
            .unwrap();

        assert_eq!(
            append.final_diagnostics.active_items, append.final_diagnostics.total_items,
            "append-only keeps everything active"
        );
        let catalog_archived = dynamic
            .items
            .iter()
            .filter(|item| item.attention == agent_contracts::AttentionState::Archived)
            .count();
        assert!(
            catalog_archived > 0 || dynamic.gc_evictions > 0,
            "dynamic engine should archive completed-task detail (catalog archived={catalog_archived}, gc_evictions={})",
            dynamic.gc_evictions
        );
        assert!(
            dynamic.final_diagnostics.active_items < dynamic.final_diagnostics.total_items
                || dynamic.final_diagnostics.warm_items + dynamic.final_diagnostics.cold_items > 0,
            "completed-task bodies must leave Active Resident, not just change attention on the heap"
        );
    }

    #[tokio::test]
    async fn pinned_constraint_survives_in_all_engines() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "pinned_constraint")
            .expect("scenario");
        for (label, outcome) in compare_scenario(&scenario, &config).await.unwrap() {
            let pinned = outcome
                .items
                .iter()
                .find(|item| item.kind == agent_contracts::ContextKind::Constraint);
            assert!(
                pinned.is_some(),
                "{label}: pinned constraint must exist as an item"
            );
            assert_eq!(
                pinned.unwrap().attention,
                agent_contracts::AttentionState::Active,
                "{label}: pinned constraint must stay active"
            );
        }
    }

    #[tokio::test]
    async fn supersession_reduces_cost_on_superseded_decisions() {
        // The experiments measured a regression on this scenario (C cost more than A/B
        // because superseded decisions were re-ingested as fresh items).
        // Supersession must fix that.
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "superseded_decisions")
            .expect("scenario");
        let v0 = run_engine(
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::baseline_v0())),
            &scenario.events,
            &config,
        )
        .await
        .unwrap();
        let p4 = run_engine(
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
            &scenario.events,
            &config,
        )
        .await
        .unwrap();

        assert!(
            p4.input_tokens_total < v0.input_tokens_total,
            "P4 must reduce input cost on superseded decisions: P4={} v0={}",
            p4.input_tokens_total,
            v0.input_tokens_total
        );
        let p4_archived = p4
            .items
            .iter()
            .filter(|item| item.attention == agent_contracts::AttentionState::Archived)
            .count();
        let v0_archived = v0
            .items
            .iter()
            .filter(|item| item.attention == agent_contracts::AttentionState::Archived)
            .count();
        assert!(
            p4_archived > v0_archived || p4.gc_evictions > v0.gc_evictions,
            "superseded decisions must leave Active Resident: P4 archived={p4_archived} evict={} v0 archived={v0_archived} evict={}",
            p4.gc_evictions,
            v0.gc_evictions
        );
        let supersessions = p4
            .items
            .iter()
            .flat_map(|item| &item.transitions)
            .filter(|t| t.reason.contains("superseded by decision"))
            .count();
        assert!(
            supersessions >= 3,
            "supersession must be observable as transitions, got {supersessions}"
        );
    }

    #[tokio::test]
    async fn failing_rounds_are_verified_and_archived_in_test_fix_loop() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "test_fix_loop")
            .expect("scenario");
        let outcome = run_engine(
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
            &scenario.events,
            &config,
        )
        .await
        .unwrap();

        // The recurring failures were superseded by each other, and the final
        // green round verified the last one: errors end up archived with an
        // explainable "verified fixed" transition, and the working set stays
        // far below the budget.
        let verified_errors = outcome
            .items
            .iter()
            .filter(|item| item.kind == agent_contracts::ContextKind::Error)
            .filter(|item| {
                item.transitions
                    .iter()
                    .any(|t| t.reason.contains("verified fixed"))
            })
            .count();
        assert!(
            verified_errors >= 1,
            "the final green round must verify the last failing log"
        );
        assert_eq!(
            outcome.over_budget_snapshots, 0,
            "error lifecycle must keep the working set within budget"
        );
    }

    #[tokio::test]
    async fn rolling_summary_bounds_but_loses_history() {
        let config = compare_config();
        for name in [
            "long_refactor",
            "test_fix_loop",
            "high_volume_irrelevant_output",
        ] {
            let scenario = all_scenarios()
                .into_iter()
                .find(|s| s.name == name)
                .expect(name);
            let results = compare_scenario(&scenario, &config).await.unwrap();
            let append = results
                .iter()
                .find(|(l, _)| *l == "A append-only")
                .map(|(_, o)| o)
                .unwrap();
            let rolling = results
                .iter()
                .find(|(l, _)| *l == "B rolling-summary")
                .map(|(_, o)| o)
                .unwrap();

            // B stays within the window thanks to collapsing — but at the
            // cost of dropping history (its retained items shrink while A's
            // keep growing).
            assert_eq!(
                rolling.over_budget_snapshots, 0,
                "{name}: rolling summary should stay within the budget"
            );
            assert!(
                rolling.input_tokens_max < append.input_tokens_max,
                "{name}: rolling peak {} should be below append-only peak {}",
                rolling.input_tokens_max,
                append.input_tokens_max
            );
            assert!(
                rolling.final_diagnostics.total_items < append.final_diagnostics.total_items,
                "{name}: rolling summary must drop history, kept {} vs append-only {}",
                rolling.final_diagnostics.total_items,
                append.final_diagnostics.total_items
            );
        }
    }

    /// The other half of the evaluation acceptance: saving tokens must not
    /// lower task success rate. In these scripted workloads a fix can only
    /// succeed if the failure facts stay visible — the dynamic engine must
    /// still select every Error item by the model rounds that need it, even
    /// as it trims the working set to a fraction of append-only's size.
    #[tokio::test]
    async fn dynamic_saves_tokens_without_losing_failure_facts() {
        let config = compare_config();
        for name in ["long_refactor", "test_fix_loop"] {
            let scenario = all_scenarios()
                .into_iter()
                .find(|s| s.name == name)
                .expect(name);
            let results = compare_scenario(&scenario, &config).await.unwrap();
            let append = results
                .iter()
                .find(|(l, _)| *l == "A append-only")
                .map(|(_, o)| o)
                .expect("append-only");
            let dynamic = results
                .iter()
                .find(|(l, _)| *l == "C dynamic")
                .map(|(_, o)| o)
                .expect("dynamic");

            // Token saving holds on this workload too.
            assert!(
                dynamic.input_tokens_total * 2 < append.input_tokens_total,
                "{name}: dynamic input {} must be below half of append-only {}",
                dynamic.input_tokens_total,
                append.input_tokens_total
            );
            assert_eq!(
                dynamic.over_budget_snapshots, 0,
                "{name}: the dynamic working set must stay within the budget"
            );

            // Success rate: every failure fact was selected by at least one
            // model round after it appeared — the fix always had the
            // failure in view while it mattered.
            let errors: Vec<_> = dynamic
                .items
                .iter()
                .filter(|item| item.kind == agent_contracts::ContextKind::Error)
                .collect();
            assert!(
                !errors.is_empty(),
                "{name}: the workload must contain failure facts"
            );
            for error in errors {
                assert!(
                    !error.consumed_turns.is_empty(),
                    "{name}: failure fact {} must be selected by a model round, got {:?}",
                    error.id,
                    error.consumed_turns
                );
            }
        }
    }
}

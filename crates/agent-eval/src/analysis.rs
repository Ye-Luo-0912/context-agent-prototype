//! EVAL-01.2 / EVAL-01.3 / EVAL-01.3b / EVAL-01.3c：正式门禁的预注册分析。
//!
//! EVAL-01.2 冻结估计量、聚类、单侧区间、ITT 规则，以及历史 30×3 功效表。
//! EVAL-01.3 在收集接受细胞之前，用同一模型重冻 n/repeats：300 题 × 3 次重复。
//! 边际保持 −5 pp。EVAL-01.3b 冻结套件并声明检索次级指标，不改门禁 n/边际。
//! EVAL-01.3c 锁死 exact 300 acceptance ids，并让 token 诊断遵守 cost_eligible。
//!
//! 不把 `--repeats` 烟雾当成独立任务。300×3 接受细胞须先做剩余校准。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bundle::{CellManifest, CellSummary};

pub const ANALYSIS_SCHEMA: &str = "agent-eval.analysis.v2";
pub const ARM_ORDER_SALT: &str = "agent-eval.arm-order.v1";
pub const SCRIPTED_ARM_ORDER: [&str; 3] = ["append", "rolling", "dynamic"];

/// 非劣效边际：C − A 成功率差的 95% 单侧 LCL 不得差过 −5 pp。
/// EVAL-01.3 不放宽边际；只改样本量。
pub const MARGIN: f64 = -0.05;
/// 历史提案（EVAL-01.2 功效表）。功效不足，不得用来收接受细胞。
pub const HISTORICAL_TASKS: usize = 30;
pub const HISTORICAL_REPEATS: u32 = 3;
/// EVAL-01.3：保守模型下 P(pass|Δ=0)≥0.80 的最小整齐三分层 r=3 设计。
/// 290×3 只有 4003/5000（刀口），300×3 才锁成门禁 n。
pub const MIN_TASKS: usize = 300;
pub const GATE_REPEATS: u32 = 3;
/// 接受套件已冻结（EVAL-01.3b）。EVAL-01.3c 锁死 exact 300 ids。
/// 300×3 接受细胞仍须先做剩余校准。
pub const SUITE_FROZEN: bool = true;

/// 日期种子 YYYY_MM_DD，不是按千分位分组的整数。
#[allow(clippy::inconsistent_digit_grouping)]
const POWER_SEED: u64 = 2026_08_14;
const POWER_SIMS: u32 = 5_000;

/// 预注册正文。改任何一个字节都会改变 `spec_sha256`。
pub const SPEC: &str = "\
schema=agent-eval.analysis.v2
primary_estimand=mean over tasks of (task-level C success rate minus task-level A success rate)
engines=dynamic minus append; rolling is reported, not in the primary contrast
task_level_rate=mean of intended repeats for that task
itt=timeout, round-cap, runtime error, missing cell => success 0; never drop the cell
cost_eligible=usage complete, journaled seq contiguous, broadcast_lagged=0; ineligible cost is omitted from token diagnostics only
interval=one-sided 95% Student-t LCL on the n_tasks paired differences
margin=-0.05
gate=LCL>=-0.05 AND n_tasks==300 AND evidence_ids==acceptance_ids AND every task has 3 repeats AND suite_frozen
repeats_are_not_tasks=true
both_pass_tokens=secondary diagnostic only
cost_missing_rate=share of intended A/C pairs that fail cost_eligible; omitted from token means; not in the primary LCL gate
arm_order=Fisher-Yates of [append,rolling,dynamic] from splitmix64(sha256(agent-eval.arm-order.v1||fixture||repeat)); live only
power_seed=20260814
power_sims=5000
power_model=thirds easy p=0.90, medium p=0.70, hard p=0.40; C=clamp(p+delta,0,1); A independent of C given task
historical_design=30 tasks x 3 repeats (10/10/10)
historical_power_d0=961/5000
historical_power_d_m05=238/5000
historical_power_d_m10=49/5000
design=300 tasks x 3 repeats (100/100/100)
power_result_d0=4048/5000
power_result_d_m05=258/5000
power_result_d_m10=0/5000
power_note=EVAL-01.3c locks exact 300 acceptance ids (pilot 30 is a subset); n/repeats/margin stay EVAL-01.3 (300x3, -5pp); pack remains 509 and is not an optional pool; SWE-bench large n=46 so harvest sizes are 107/147/46; power-model thirds stay index-based; do not collect 300x3 acceptance cells until remaining calibration; do not invent tasks
suite_frozen=true
suite_pack=agent-eval.suite.v1 n=509 (9 file + 500 princeton-nlp/SWE-bench_Verified)
acceptance_ids=agent-eval.acceptance.v1 n=300 sha256=7ff6b5ddefc7e6e6dc138e5e582de75b0cfc4f5eba831385cc550e4df8c124a7
retrieval_secondaries=search recall/latency, found-after-forgotten, graded-access distribution; same A/B/C cells; not in the primary LCL gate
live_protocol=eval-01.5.p1
live_send_window=provider context_window (eval live default 128000; OPENAI_CONTEXT_WINDOW overrides); kernel 24000 is not the send fallback when a window is declared
live_pack_budget=min(kernel context_budget_tokens, send window); C/B pack against this
live_A=append retains history; runtime trims to send window (grows until window); not a smaller send than C
live_rounds=48 shared A/B/C (harness and kernel max_tool_rounds); do not give C a higher cap than A; C extra rounds under the cap are a treatment effect
p0_protocol=send=kernel-fallback 19904 rounds=12; file-only 81 cells plus SWE-bench floor cells; do not mix p0 and p1 in one ITT table
";

#[derive(Debug, Clone)]
pub struct CellRecord {
    pub fixture_id: String,
    pub repeat: u32,
    pub engine: String,
    pub passed: bool,
    pub outcome: String,
    pub error: Option<String>,
    pub usage_incomplete: bool,
    pub seq_contiguous: bool,
    pub broadcast_lagged: u64,
    pub model_input_tokens: u64,
    pub rounds: u64,
    pub tool_calls: u64,
    pub missing: bool,
}

#[derive(Debug, Clone)]
pub struct TaskPair {
    pub fixture_id: String,
    pub repeats: u32,
    pub a_successes: u32,
    pub c_successes: u32,
    pub a_rate: f64,
    pub c_rate: f64,
    pub diff: f64,
}

#[derive(Debug, Clone)]
pub struct Interval {
    pub n_tasks: usize,
    pub mean: f64,
    pub se: f64,
    pub df: u32,
    pub t_crit: f64,
    pub lcl: f64,
    pub degenerate: bool,
}

#[derive(Debug, Clone)]
pub struct PowerReport {
    pub seed: u64,
    pub sims: u32,
    pub n_pass_delta_0: u32,
    pub n_pass_delta_m05: u32,
    pub n_pass_delta_m10: u32,
}

#[derive(Debug, Clone)]
pub struct CostSummary {
    pub pairs: u32,
    pub mean_a_input: f64,
    pub mean_c_input: f64,
    pub mean_c_minus_a: f64,
    pub mean_a_rounds: f64,
    pub mean_c_rounds: f64,
    pub mean_a_tools: f64,
    pub mean_c_tools: f64,
}

#[derive(Debug, Clone)]
pub struct CostMissing {
    pub intended_pairs: u32,
    pub eligible_pairs: u32,
    pub missing_pairs: u32,
    pub missing_rate: f64,
}

#[derive(Debug, Clone)]
pub struct GateReport {
    pub spec_sha256: String,
    pub suite_frozen: bool,
    pub eligible: bool,
    pub ineligible_reasons: Vec<String>,
    pub decision: &'static str,
    pub interval: Option<Interval>,
    pub tasks: Vec<TaskPair>,
    pub power: PowerReport,
    pub cost_eligible_cost: Option<CostSummary>,
    pub both_pass_cost: Option<CostSummary>,
    pub cost_missing: Option<CostMissing>,
    pub outcomes: BTreeMap<&'static str, u32>,
}

impl CellRecord {
    pub fn itt_success(&self) -> bool {
        !self.missing && self.passed && self.error.is_none()
    }

    pub fn cost_eligible(&self) -> bool {
        !self.missing && !self.usage_incomplete && self.seq_contiguous && self.broadcast_lagged == 0
    }
}

pub fn spec_sha256() -> String {
    hex_encode(Sha256::digest(SPEC.as_bytes()))
}

/// Live 细胞的引擎顺序：按 fixture × repeat 打乱，抵消供应商时间漂移。
/// 脚本化 `--compare-arm` 仍用固定 append → rolling → dynamic。
pub fn arm_order(fixture_id: &str, repeat: u32) -> [&'static str; 3] {
    let mut arms = SCRIPTED_ARM_ORDER;
    let mut hasher = Sha256::new();
    hasher.update(ARM_ORDER_SALT.as_bytes());
    hasher.update([0]);
    hasher.update(fixture_id.as_bytes());
    hasher.update([0]);
    hasher.update(repeat.to_string().as_bytes());
    let digest = hasher.finalize();
    let seed = u64::from_le_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"));
    let mut rng = SplitMix64(seed);
    for i in (1..arms.len()).rev() {
        let j = rng.bounded(i as u64 + 1) as usize;
        arms.swap(i, j);
    }
    arms
}

pub fn one_sided_lcl(diffs: &[f64]) -> Option<Interval> {
    let n = diffs.len();
    if n < 2 {
        return None;
    }
    let n_f = n as f64;
    let mean = diffs.iter().sum::<f64>() / n_f;
    let var = diffs
        .iter()
        .map(|d| {
            let e = d - mean;
            e * e
        })
        .sum::<f64>()
        / (n_f - 1.0);
    let df = (n - 1) as u32;
    let t_crit = t_one_sided_95(df);
    if !var.is_finite() || var == 0.0 {
        return Some(Interval {
            n_tasks: n,
            mean,
            se: 0.0,
            df,
            t_crit,
            lcl: mean,
            degenerate: true,
        });
    }
    let se = var.sqrt() / n_f.sqrt();
    Some(Interval {
        n_tasks: n,
        mean,
        se,
        df,
        t_crit,
        lcl: mean - t_crit * se,
        degenerate: false,
    })
}

pub fn power_simulation() -> PowerReport {
    power_simulation_for(MIN_TASKS, GATE_REPEATS)
}

pub fn historical_power_simulation() -> PowerReport {
    power_simulation_for(HISTORICAL_TASKS, HISTORICAL_REPEATS)
}

pub fn power_simulation_for(n_tasks: usize, repeats: u32) -> PowerReport {
    let mut rng = SplitMix64(POWER_SEED);
    let mut n_pass_delta_0 = 0;
    let mut n_pass_delta_m05 = 0;
    let mut n_pass_delta_m10 = 0;
    for _ in 0..POWER_SIMS {
        if simulate_gate(&mut rng, n_tasks, repeats, 0.0) {
            n_pass_delta_0 += 1;
        }
        if simulate_gate(&mut rng, n_tasks, repeats, -0.05) {
            n_pass_delta_m05 += 1;
        }
        if simulate_gate(&mut rng, n_tasks, repeats, -0.10) {
            n_pass_delta_m10 += 1;
        }
    }
    PowerReport {
        seed: POWER_SEED,
        sims: POWER_SIMS,
        n_pass_delta_0,
        n_pass_delta_m05,
        n_pass_delta_m10,
    }
}

pub fn analyze(cells: &[CellRecord]) -> GateReport {
    let tasks = cluster_tasks(cells);
    let diffs: Vec<f64> = tasks.iter().map(|task| task.diff).collect();
    let interval = one_sided_lcl(&diffs);
    let mut reasons = Vec::new();
    if !SUITE_FROZEN {
        let pack_note = crate::suite::load_pack()
            .map(|pack| format!("suite pack {}/{}", pack.tasks.len(), crate::suite::TARGET_N))
            .unwrap_or_else(|_| "suite pack unreadable".into());
        reasons.push(format!(
            "acceptance suite is not frozen ({pack_note}; 5 smoke/diagnostic fixtures, not {MIN_TASKS} tasks)"
        ));
    } else {
        match crate::suite::load_pack() {
            Ok(pack) if pack.frozen() => {}
            Ok(_) => reasons.push("SUITE_FROZEN but suite pack is not frozen".into()),
            Err(_) => reasons.push("SUITE_FROZEN but suite pack unreadable".into()),
        }
    }
    if tasks.len() != MIN_TASKS {
        reasons.push(format!("n_tasks={} != {MIN_TASKS}", tasks.len()));
    }
    let observed_ids: BTreeSet<String> = tasks.iter().map(|task| task.fixture_id.clone()).collect();
    reasons.extend(crate::acceptance::evidence_id_reasons(&observed_ids));
    match observed_repeats(&tasks) {
        Some(repeats) if repeats == GATE_REPEATS => {}
        Some(repeats) => reasons.push(format!("repeats={repeats} (gate requires {GATE_REPEATS})")),
        None if tasks.is_empty() => reasons.push("no paired tasks in the evidence root".into()),
        None => reasons.push("tasks do not share one repeat count".into()),
    }
    if interval.is_none() {
        reasons.push("need at least 2 tasks to form a t interval".into());
    }
    let eligible = reasons.is_empty();
    let decision = if !eligible {
        "ineligible"
    } else if interval
        .as_ref()
        .is_some_and(|interval| interval.lcl >= MARGIN)
    {
        "pass"
    } else {
        "fail"
    };
    GateReport {
        spec_sha256: spec_sha256(),
        suite_frozen: SUITE_FROZEN,
        eligible,
        ineligible_reasons: reasons,
        decision,
        interval,
        cost_eligible_cost: cost_summary(cells, false),
        both_pass_cost: cost_summary(cells, true),
        cost_missing: cost_missing(cells),
        tasks,
        power: power_simulation(),
        outcomes: outcome_histogram(cells),
    }
}

pub fn load_evidence_root(root: &Path) -> anyhow::Result<Vec<CellRecord>> {
    if root.join("pair.json").is_file() {
        return load_pair_dir(root);
    }
    let mut cells = Vec::new();
    if !root.is_dir() {
        anyhow::bail!("evidence root is not a directory: {}", root.display());
    }
    let mut fixtures: Vec<_> = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    fixtures.sort_by_key(|entry| entry.file_name());
    for fixture in fixtures {
        let path = fixture.path();
        if !path.is_dir() {
            continue;
        }
        let mut repeats: Vec<_> = fs::read_dir(&path)?.collect::<Result<Vec<_>, _>>()?;
        repeats.sort_by_key(|entry| entry.file_name());
        for repeat in repeats {
            let repeat_path = repeat.path();
            if repeat_path.join("pair.json").is_file() {
                cells.extend(load_pair_dir(&repeat_path)?);
            }
        }
    }
    Ok(cells)
}

pub fn render_preregister() -> String {
    let design = power_simulation();
    let historical = historical_power_simulation();
    let mut out = String::new();
    out.push_str(&format!(
        "{ANALYSIS_SCHEMA} spec_sha256={}\n",
        spec_sha256()
    ));
    out.push_str(&format!("suite_frozen={SUITE_FROZEN}\n"));
    out.push_str(&format!(
        "smoke FIXTURES: {}/{} (diagnostic only; pack is the acceptance suite)\n",
        crate::workload::FIXTURES.len(),
        MIN_TASKS
    ));
    if let Ok(pack) = crate::suite::load_pack() {
        out.push_str(&format!(
            "suite pack: {}/{} frozen={} blockers={}\n",
            pack.tasks.len(),
            pack.manifest.target_n,
            pack.frozen(),
            pack.blockers.len()
        ));
    }
    out.push_str(SPEC);
    out.push('\n');
    out.push_str("historical 30x3 (EVAL-01.2, not the gate):\n");
    out.push_str(&render_power(&historical));
    out.push_str("design 300x3 (EVAL-01.3 gate n/repeats):\n");
    out.push_str(&render_power(&design));
    out.push_str(
        "power note: n/repeats/margin frozen; suite frozen; exact 300 acceptance ids locked; retrieval secondaries declared; do not collect 300x3 acceptance cells until remaining calibration; this is not an M15 close.\n",
    );
    if let Ok(pack) = crate::suite::load_pack() {
        if let Ok(sample) = crate::pilot::select_pilot(&pack) {
            out.push_str(&format!(
                "calibration sample: n={} sha256={} (--pilot / --pilot-run / --pilot-calibrate; decision=pilot)\n",
                sample.tasks.len(),
                sample.sha256
            ));
        }
        if let Ok(ids) = crate::acceptance::select_acceptance(&pack) {
            out.push_str(&format!(
                "acceptance ids: n={} sha256={} (--acceptance; gate requires this exact set)\n",
                ids.len(),
                crate::acceptance::FROZEN_ACCEPTANCE_SHA256
            ));
        }
    }
    out
}

pub fn render_report(report: &GateReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{ANALYSIS_SCHEMA} spec_sha256={} decision={}\n",
        report.spec_sha256, report.decision
    ));
    out.push_str(&format!(
        "suite_frozen={} eligible={} n_tasks={}\n",
        report.suite_frozen,
        report.eligible,
        report.tasks.len()
    ));
    for reason in &report.ineligible_reasons {
        out.push_str(&format!("  ineligible: {reason}\n"));
    }
    for (name, count) in &report.outcomes {
        out.push_str(&format!("  outcome {name}={count}\n"));
    }
    if let Some(interval) = &report.interval {
        out.push_str(&format!(
            "primary C-A: n_tasks={} mean={:.6} se={:.6} df={} t={:.6} LCL={:.6} degenerate={} margin={MARGIN}\n",
            interval.n_tasks,
            interval.mean,
            interval.se,
            interval.df,
            interval.t_crit,
            interval.lcl,
            interval.degenerate
        ));
    }
    for task in &report.tasks {
        out.push_str(&format!(
            "  {:20} repeats={} A={}/{} ({:.3}) C={}/{} ({:.3}) d={:+.3}\n",
            task.fixture_id,
            task.repeats,
            task.a_successes,
            task.repeats,
            task.a_rate,
            task.c_successes,
            task.repeats,
            task.c_rate,
            task.diff
        ));
    }
    if let Some(missing) = &report.cost_missing {
        out.push_str(&format!(
            "cost-missing rate: intended={} eligible={} missing={} rate={:.3} (usage incomplete / seq gap / broadcast lagged / missing arm; not in the LCL gate)\n",
            missing.intended_pairs,
            missing.eligible_pairs,
            missing.missing_pairs,
            missing.missing_rate
        ));
    }
    if let Some(cost) = &report.cost_eligible_cost {
        out.push_str(&format!(
            "cost-eligible tokens (paired, usage complete): n={} mean_A={:.1} mean_C={:.1} C-A={:+.1} rounds A/C={:.1}/{:.1} tools A/C={:.1}/{:.1}\n",
            cost.pairs, cost.mean_a_input, cost.mean_c_input, cost.mean_c_minus_a,
            cost.mean_a_rounds, cost.mean_c_rounds, cost.mean_a_tools, cost.mean_c_tools
        ));
    } else {
        out.push_str("cost-eligible tokens (paired, usage complete): no eligible pairs\n");
    }
    if let Some(cost) = &report.both_pass_cost {
        out.push_str(&format!(
            "both-pass tokens (secondary): n={} mean_A={:.1} mean_C={:.1} C-A={:+.1} rounds A/C={:.1}/{:.1} tools A/C={:.1}/{:.1}\n",
            cost.pairs, cost.mean_a_input, cost.mean_c_input, cost.mean_c_minus_a,
            cost.mean_a_rounds, cost.mean_c_rounds, cost.mean_a_tools, cost.mean_c_tools
        ));
    } else {
        out.push_str("both-pass tokens (secondary): no both-pass pairs\n");
    }
    out.push_str(&render_power(&report.power));
    out.push_str(
        "this is still not an M15 close: the gate also requires the exact frozen 300 ids and a model-backed B.\n",
    );
    out
}

fn render_power(power: &PowerReport) -> String {
    format!(
        "power sims={} seed={} P(pass|d=0)={:.4} ({}/{}) P(pass|d=-0.05)={:.4} ({}/{}) P(pass|d=-0.10)={:.4} ({}/{})\n",
        power.sims,
        power.seed,
        power.n_pass_delta_0 as f64 / power.sims as f64,
        power.n_pass_delta_0,
        power.sims,
        power.n_pass_delta_m05 as f64 / power.sims as f64,
        power.n_pass_delta_m05,
        power.sims,
        power.n_pass_delta_m10 as f64 / power.sims as f64,
        power.n_pass_delta_m10,
        power.sims
    )
}

fn load_pair_dir(dir: &Path) -> anyhow::Result<Vec<CellRecord>> {
    let pair: Value = serde_json::from_str(&fs::read_to_string(dir.join("pair.json"))?)?;
    let fixture_id = pair
        .get("fixture_id")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let repeat = pair.get("repeat").and_then(Value::as_u64).unwrap_or(0) as u32;
    let engines: Vec<String> = pair
        .get("arm_order")
        .and_then(Value::as_array)
        .or_else(|| pair.get("cells").and_then(Value::as_array))
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    row.as_str().map(str::to_string).or_else(|| {
                        row.get("engine")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                })
                .collect()
        })
        .unwrap_or_else(|| {
            SCRIPTED_ARM_ORDER
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        });
    let mut cells = Vec::new();
    for engine in engines {
        let cell_dir = dir.join(&engine);
        if !cell_dir.join("summary.json").is_file() {
            cells.push(CellRecord {
                fixture_id: fixture_id.clone(),
                repeat,
                engine,
                passed: false,
                outcome: "missing".into(),
                error: Some("missing bundle".into()),
                usage_incomplete: true,
                seq_contiguous: false,
                broadcast_lagged: 0,
                model_input_tokens: 0,
                rounds: 0,
                tool_calls: 0,
                missing: true,
            });
            continue;
        }
        cells.push(load_cell(&cell_dir, &fixture_id, repeat, &engine)?);
    }
    Ok(cells)
}

fn load_cell(
    dir: &Path,
    fixture_id: &str,
    repeat: u32,
    engine: &str,
) -> anyhow::Result<CellRecord> {
    let summary: CellSummary =
        serde_json::from_str(&fs::read_to_string(dir.join("summary.json"))?)?;
    let fixture_id = if dir.join("manifest.json").is_file() {
        let manifest: CellManifest =
            serde_json::from_str(&fs::read_to_string(dir.join("manifest.json"))?)?;
        manifest.fixture_id
    } else {
        fixture_id.to_string()
    };
    Ok(CellRecord {
        fixture_id,
        repeat,
        engine: engine.to_string(),
        passed: summary.passed,
        outcome: summary.outcome,
        error: summary.error,
        usage_incomplete: summary.usage_incomplete,
        seq_contiguous: summary.seq_contiguous,
        broadcast_lagged: summary.broadcast_lagged,
        model_input_tokens: metric_u64(&summary.metrics, "model_input_tokens"),
        rounds: metric_u64(&summary.metrics, "rounds"),
        tool_calls: metric_u64(&summary.metrics, "tool_calls"),
        missing: false,
    })
}

fn outcome_histogram(cells: &[CellRecord]) -> BTreeMap<&'static str, u32> {
    let mut counts = BTreeMap::new();
    for cell in cells {
        *counts.entry(classify_outcome(cell)).or_default() += 1;
    }
    counts
}

fn classify_outcome(cell: &CellRecord) -> &'static str {
    if cell.missing {
        "missing"
    } else if cell
        .error
        .as_deref()
        .is_some_and(|error| error.contains("timed out"))
    {
        "timeout"
    } else if cell
        .error
        .as_deref()
        .is_some_and(|error| error.contains("round cap"))
    {
        "round_cap"
    } else if cell.outcome == "error" {
        "error"
    } else if cell.itt_success() {
        "pass"
    } else {
        "verify_failed"
    }
}

fn cluster_tasks(cells: &[CellRecord]) -> Vec<TaskPair> {
    let mut by_fixture: BTreeMap<String, Vec<&CellRecord>> = BTreeMap::new();
    for cell in cells {
        by_fixture
            .entry(cell.fixture_id.clone())
            .or_default()
            .push(cell);
    }
    by_fixture
        .into_iter()
        .map(|(fixture_id, rows)| {
            let repeats: BTreeSet<u32> = rows.iter().map(|cell| cell.repeat).collect();
            let n = repeats.len() as u32;
            let mut a_successes = 0;
            let mut c_successes = 0;
            for repeat in &repeats {
                if itt_at(&rows, "append", *repeat) {
                    a_successes += 1;
                }
                if itt_at(&rows, "dynamic", *repeat) {
                    c_successes += 1;
                }
            }
            let a_rate = if n == 0 {
                0.0
            } else {
                a_successes as f64 / n as f64
            };
            let c_rate = if n == 0 {
                0.0
            } else {
                c_successes as f64 / n as f64
            };
            TaskPair {
                fixture_id,
                repeats: n,
                a_successes,
                c_successes,
                a_rate,
                c_rate,
                diff: c_rate - a_rate,
            }
        })
        .collect()
}

fn itt_at(rows: &[&CellRecord], engine: &str, repeat: u32) -> bool {
    rows.iter()
        .find(|cell| cell.engine == engine && cell.repeat == repeat)
        .map(|cell| cell.itt_success())
        .unwrap_or(false)
}

fn observed_repeats(tasks: &[TaskPair]) -> Option<u32> {
    let first = tasks.first()?.repeats;
    if first > 0 && tasks.iter().all(|task| task.repeats == first) {
        Some(first)
    } else {
        None
    }
}

fn paired_ac<'a>(
    cells: &'a [CellRecord],
) -> impl Iterator<Item = (&'a CellRecord, &'a CellRecord)> {
    let mut by_key: BTreeMap<(String, u32), Vec<&'a CellRecord>> = BTreeMap::new();
    for cell in cells {
        by_key
            .entry((cell.fixture_id.clone(), cell.repeat))
            .or_default()
            .push(cell);
    }
    by_key.into_values().filter_map(|rows| {
        let a = rows.iter().copied().find(|cell| cell.engine == "append")?;
        let c = rows.iter().copied().find(|cell| cell.engine == "dynamic")?;
        Some((a, c))
    })
}

fn cost_summary(cells: &[CellRecord], both_pass: bool) -> Option<CostSummary> {
    let mut n = 0u32;
    let mut sum_a = 0.0;
    let mut sum_c = 0.0;
    let mut sum_a_rounds = 0.0;
    let mut sum_c_rounds = 0.0;
    let mut sum_a_tools = 0.0;
    let mut sum_c_tools = 0.0;
    for (a, c) in paired_ac(cells) {
        if !a.cost_eligible() || !c.cost_eligible() {
            continue;
        }
        if both_pass && (!a.itt_success() || !c.itt_success()) {
            continue;
        }
        n += 1;
        sum_a += a.model_input_tokens as f64;
        sum_c += c.model_input_tokens as f64;
        sum_a_rounds += a.rounds as f64;
        sum_c_rounds += c.rounds as f64;
        sum_a_tools += a.tool_calls as f64;
        sum_c_tools += c.tool_calls as f64;
    }
    if n == 0 {
        return None;
    }
    let mean_a_input = sum_a / n as f64;
    let mean_c_input = sum_c / n as f64;
    Some(CostSummary {
        pairs: n,
        mean_a_input,
        mean_c_input,
        mean_c_minus_a: mean_c_input - mean_a_input,
        mean_a_rounds: sum_a_rounds / n as f64,
        mean_c_rounds: sum_c_rounds / n as f64,
        mean_a_tools: sum_a_tools / n as f64,
        mean_c_tools: sum_c_tools / n as f64,
    })
}

fn cost_missing(cells: &[CellRecord]) -> Option<CostMissing> {
    let mut intended_pairs = 0u32;
    let mut eligible_pairs = 0u32;
    for (a, c) in paired_ac(cells) {
        intended_pairs += 1;
        if a.cost_eligible() && c.cost_eligible() {
            eligible_pairs += 1;
        }
    }
    if intended_pairs == 0 {
        return None;
    }
    let missing_pairs = intended_pairs - eligible_pairs;
    Some(CostMissing {
        intended_pairs,
        eligible_pairs,
        missing_pairs,
        missing_rate: missing_pairs as f64 / intended_pairs as f64,
    })
}

fn simulate_gate(rng: &mut SplitMix64, n_tasks: usize, repeats: u32, delta: f64) -> bool {
    if n_tasks < 2 || repeats == 0 {
        return false;
    }
    let mut diffs = Vec::with_capacity(n_tasks);
    for task in 0..n_tasks {
        let p = strata_p(task, n_tasks);
        let pc = (p + delta).clamp(0.0, 1.0);
        let mut a = 0.0;
        let mut c = 0.0;
        for _ in 0..repeats {
            if rng.bernoulli(p) {
                a += 1.0;
            }
            if rng.bernoulli(pc) {
                c += 1.0;
            }
        }
        diffs.push(c / repeats as f64 - a / repeats as f64);
    }
    one_sided_lcl(&diffs)
        .map(|interval| interval.lcl >= MARGIN)
        .unwrap_or(false)
}

/// 三分层：易 0.90 / 中 0.70 / 难 0.40。n=30 时仍是 10/10/10，与 EVAL-01.2 表一致。
fn strata_p(task: usize, n_tasks: usize) -> f64 {
    let third = n_tasks / 3;
    if task < third {
        0.90
    } else if task < third.saturating_mul(2) {
        0.70
    } else {
        0.40
    }
}

fn metric_u64(metrics: &Value, key: &str) -> u64 {
    metrics.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// 单侧 95% t 临界值（P(T > t) = 0.05）。df 1..=40 用表；更大用 Cornish-Fisher。
fn t_one_sided_95(df: u32) -> f64 {
    const TABLE: [f64; 40] = [
        6.313752, 2.919986, 2.353363, 2.131847, 2.015048, 1.943180, 1.894579, 1.859548, 1.833113,
        1.812461, 1.795885, 1.782288, 1.770933, 1.761310, 1.753050, 1.745884, 1.739607, 1.734064,
        1.729133, 1.724718, 1.720743, 1.717144, 1.713872, 1.710882, 1.708141, 1.705618, 1.703288,
        1.701131, 1.699127, 1.697261, 1.695519, 1.693889, 1.692360, 1.690924, 1.689572, 1.688298,
        1.687094, 1.685954, 1.684875, 1.683851,
    ];
    if df == 0 {
        return f64::INFINITY;
    }
    if df <= 40 {
        return TABLE[df as usize - 1];
    }
    let z = 1.6448536269514722;
    let n = df as f64;
    let z3 = z * z * z;
    let z5 = z3 * z * z;
    z + (z3 + z) / (4.0 * n) + (5.0 * z5 + 16.0 * z3 + 3.0 * z) / (96.0 * n * n)
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn bounded(&mut self, max: u64) -> u64 {
        if max == 0 {
            return 0;
        }
        self.next_u64() % max
    }

    fn bernoulli(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        let u = (self.next_u64() as f64) * (1.0 / ((u64::MAX as f64) + 1.0));
        u < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(fixture: &str, repeat: u32, engine: &str, passed: bool) -> CellRecord {
        CellRecord {
            fixture_id: fixture.into(),
            repeat,
            engine: engine.into(),
            passed,
            outcome: if passed {
                "passed".into()
            } else {
                "verify_failed".into()
            },
            error: None,
            usage_incomplete: false,
            seq_contiguous: true,
            broadcast_lagged: 0,
            model_input_tokens: if engine == "dynamic" { 20 } else { 10 },
            rounds: 1,
            tool_calls: 1,
            missing: false,
        }
    }

    #[test]
    fn t_crit_matches_known_values() {
        assert!((t_one_sided_95(1) - 6.313752).abs() < 1e-6);
        assert!((t_one_sided_95(29) - 1.699127).abs() < 1e-6);
        assert!((t_one_sided_95(2) - 2.919986).abs() < 1e-6);
    }

    #[test]
    fn lcl_on_a_hand_computed_three_task_example() {
        let interval = one_sided_lcl(&[0.0, -1.0, 0.0]).unwrap();
        assert!((interval.mean + 1.0 / 3.0).abs() < 1e-12);
        assert!((interval.se - 1.0 / 3.0).abs() < 1e-12);
        assert!(!interval.degenerate);
        let expected = -1.0 / 3.0 - 2.919986 / 3.0;
        assert!((interval.lcl - expected).abs() < 1e-6);
    }

    #[test]
    fn zero_variance_lcl_equals_the_mean() {
        let interval = one_sided_lcl(&[0.0; 30]).unwrap();
        assert!(interval.degenerate);
        assert!((interval.lcl).abs() < 1e-15);
        assert!(interval.lcl >= MARGIN);
    }

    #[test]
    fn missing_and_timeout_count_as_itt_failures() {
        let cells = vec![
            cell("t1", 1, "append", true),
            cell("t1", 1, "dynamic", true),
            cell("t1", 2, "append", true),
            CellRecord {
                error: Some("turn 1 failed: fixture turn timed out".into()),
                outcome: "error".into(),
                passed: false,
                ..cell("t1", 2, "dynamic", false)
            },
            cell("t1", 3, "append", true),
            CellRecord {
                missing: true,
                passed: false,
                outcome: "missing".into(),
                error: Some("missing bundle".into()),
                ..cell("t1", 3, "dynamic", false)
            },
        ];
        let tasks = cluster_tasks(&cells);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].a_successes, 3);
        assert_eq!(tasks[0].c_successes, 1);
        assert!((tasks[0].diff - (1.0 / 3.0 - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn current_suite_cannot_pass_the_gate() {
        let mut cells = Vec::new();
        for fixture in crate::workload::FIXTURES {
            for repeat in 1..=3 {
                cells.push(cell(fixture.id, repeat, "append", true));
                cells.push(cell(fixture.id, repeat, "rolling", true));
                cells.push(cell(fixture.id, repeat, "dynamic", true));
            }
        }
        let report = analyze(&cells);
        assert!(!report.eligible);
        assert_eq!(report.decision, "ineligible");
        assert!(report.suite_frozen);
        assert!(
            !report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("not frozen"))
        );
        assert!(
            report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("n_tasks="))
        );
    }

    #[test]
    fn arm_order_is_a_permutation_and_varies_across_repeats() {
        let mut seen = BTreeSet::new();
        for fixture in ["fix_off_by_one", "recall_after_fix", "add_test"] {
            for repeat in 1..=3 {
                let order = arm_order(fixture, repeat);
                let mut sorted = order;
                sorted.sort();
                assert_eq!(sorted, ["append", "dynamic", "rolling"]);
                seen.insert(order.join(","));
            }
        }
        assert!(
            seen.len() > 1,
            "counterbalance must not collapse to one permutation: {seen:?}"
        );
        assert_eq!(
            arm_order("fix_off_by_one", 1),
            arm_order("fix_off_by_one", 1)
        );
    }

    #[test]
    fn power_simulation_is_ordered_and_frozen() {
        let first = historical_power_simulation();
        let second = historical_power_simulation();
        assert_eq!(first.n_pass_delta_0, second.n_pass_delta_0);
        assert_eq!(first.n_pass_delta_m05, second.n_pass_delta_m05);
        assert_eq!(first.n_pass_delta_m10, second.n_pass_delta_m10);
        assert_eq!(first.n_pass_delta_0, 961);
        assert_eq!(first.n_pass_delta_m05, 238);
        assert_eq!(first.n_pass_delta_m10, 49);
    }

    #[test]
    fn eval_01_3_design_is_300x3_and_meets_eighty_percent() {
        assert_eq!(MIN_TASKS, 300);
        assert_eq!(GATE_REPEATS, 3);
        assert!((MARGIN + 0.05).abs() < 1e-15);
        let design = power_simulation();
        assert_eq!(design.n_pass_delta_0, 4048);
        assert_eq!(design.n_pass_delta_m05, 258);
        assert_eq!(design.n_pass_delta_m10, 0);
        assert!(design.n_pass_delta_0 as f64 / design.sims as f64 >= 0.80);
        // 刀口 290×3 不得被误当成门禁 n。
        let knife = power_simulation_for(290, 3);
        assert_eq!(knife.n_pass_delta_0, 4003);
    }

    #[test]
    fn frozen_full_size_synthetic_cells_are_gate_eligible() {
        let mut cells = Vec::new();
        for id in crate::acceptance::frozen_ids() {
            for repeat in 1..=GATE_REPEATS {
                cells.push(cell(id, repeat, "append", true));
                cells.push(cell(id, repeat, "rolling", true));
                cells.push(cell(id, repeat, "dynamic", true));
            }
        }
        let report = analyze(&cells);
        assert!(report.suite_frozen);
        assert_eq!(report.tasks.len(), MIN_TASKS);
        assert!(
            !report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("not frozen")),
            "{:?}",
            report.ineligible_reasons
        );
        assert!(
            !report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("n_tasks="))
        );
        assert!(
            !report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("evidence ids")),
            "{:?}",
            report.ineligible_reasons
        );
        assert!(report.eligible, "{:?}", report.ineligible_reasons);
        assert_eq!(report.decision, "pass");
    }

    #[test]
    fn any_300_from_the_pack_is_not_gate_eligible() {
        let mut cells = Vec::new();
        for task in 0..MIN_TASKS {
            let id = format!("t{task:03}");
            for repeat in 1..=GATE_REPEATS {
                cells.push(cell(&id, repeat, "append", true));
                cells.push(cell(&id, repeat, "rolling", true));
                cells.push(cell(&id, repeat, "dynamic", true));
            }
        }
        let report = analyze(&cells);
        assert_eq!(report.tasks.len(), MIN_TASKS);
        assert!(!report.eligible);
        assert_eq!(report.decision, "ineligible");
        assert!(
            report
                .ineligible_reasons
                .iter()
                .any(|reason| reason.contains("evidence ids")),
            "{:?}",
            report.ineligible_reasons
        );
    }

    #[test]
    fn usage_incomplete_zeros_are_omitted_from_cost_not_from_itt() {
        let mut cells = vec![
            CellRecord {
                model_input_tokens: 100,
                ..cell("ok", 1, "append", true)
            },
            CellRecord {
                model_input_tokens: 200,
                ..cell("ok", 1, "dynamic", true)
            },
            CellRecord {
                model_input_tokens: 0,
                usage_incomplete: true,
                passed: false,
                outcome: "verify_failed".into(),
                ..cell("uuid-like", 1, "append", false)
            },
            CellRecord {
                model_input_tokens: 0,
                usage_incomplete: true,
                passed: false,
                outcome: "verify_failed".into(),
                ..cell("uuid-like", 1, "dynamic", false)
            },
        ];
        cells.push(cell("ok", 1, "rolling", true));
        cells.push(cell("uuid-like", 1, "rolling", false));
        let report = analyze(&cells);
        assert_eq!(report.tasks.len(), 2);
        let uuid = report
            .tasks
            .iter()
            .find(|task| task.fixture_id == "uuid-like")
            .unwrap();
        assert_eq!(uuid.a_successes, 0);
        assert_eq!(uuid.c_successes, 0);
        let missing = report.cost_missing.as_ref().unwrap();
        assert_eq!(missing.intended_pairs, 2);
        assert_eq!(missing.eligible_pairs, 1);
        assert!((missing.missing_rate - 0.5).abs() < 1e-12);
        let cost = report.cost_eligible_cost.as_ref().unwrap();
        assert_eq!(cost.pairs, 1);
        assert!((cost.mean_a_input - 100.0).abs() < 1e-12);
        assert!((cost.mean_c_input - 200.0).abs() < 1e-12);
        let text = render_report(&report);
        assert!(text.contains("cost-eligible tokens"));
        assert!(!text.contains("itt tokens (all intended pairs)"));
    }

    #[test]
    fn spec_hash_is_stable() {
        assert_eq!(spec_sha256(), spec_sha256());
        assert_eq!(spec_sha256().len(), 64);
        assert_eq!(
            spec_sha256(),
            "2469ebcab44b6246ed35f1c82a35574d262ecb4b3912b858eb13dca407c344c2"
        );
        assert_eq!(ANALYSIS_SCHEMA, crate::bundle::ANALYSIS_SCHEMA);
        assert!(SPEC.contains("schema=agent-eval.analysis.v2"));
        assert!(SPEC.contains("historical_power_d0=961/5000"));
        assert!(SPEC.contains("power_result_d0=4048/5000"));
        assert!(SPEC.contains("design=300 tasks x 3 repeats"));
        assert!(SPEC.contains("suite_frozen=true"));
        assert!(SPEC.contains("retrieval_secondaries="));
        assert!(SPEC.contains("margin=-0.05"));
        assert!(SPEC.contains("n_tasks==300"));
        assert!(SPEC.contains("evidence_ids==acceptance_ids"));
        assert!(SPEC.contains("acceptance_ids=agent-eval.acceptance.v1"));
        assert!(SPEC.contains("cost_missing_rate="));
        assert!(SPEC.contains("live_protocol=eval-01.5.p1"));
        assert!(SPEC.contains("live_rounds=48 shared"));
        assert!(SPEC.contains("p0_protocol="));
        assert!(SPEC.contains(crate::acceptance::FROZEN_ACCEPTANCE_SHA256));
        assert!(crate::workload::render_fixtures().contains(&format!(
            "{}/{}",
            crate::workload::FIXTURES.len(),
            MIN_TASKS
        )));
    }

    #[test]
    fn loads_a_written_pair_as_itt_cells() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = &crate::workload::FIXTURES[0];
        let pair = crate::bundle::PairSink {
            root: tmp.path().join("evidence"),
            fixture_id: fixture.id.to_string(),
            repeat: 1,
            repeats: 1,
            live: false,
        };
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let events = Vec::new();
        let metrics = crate::metrics::RunMetrics::default();
        for engine in ["append", "rolling", "dynamic"] {
            crate::bundle::write_cell(
                &pair.cell_dir(engine),
                fixture,
                engine,
                &pair,
                &events,
                &metrics,
                engine != "dynamic",
                1,
                None,
                &workspace,
                0,
                0,
            )
            .unwrap();
        }
        crate::bundle::write_pair(&pair, &["append", "rolling", "dynamic"]).unwrap();
        let cells = load_evidence_root(&pair.root).unwrap();
        assert_eq!(cells.len(), 3);
        let report = analyze(&cells);
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.tasks[0].a_successes, 1);
        assert_eq!(report.tasks[0].c_successes, 0);
        assert_eq!(report.decision, "ineligible");
    }
}

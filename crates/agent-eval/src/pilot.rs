//! EVAL-01.5：冻结的 ~30×3 非接受校准样本。
//!
//! 在看到任何校准细胞之前锁死 30 个 id：9 道文件题全收，再按
//! `sha256(agent-eval.pilot.v1 || POWER_SEED || id)` 从 SWE-bench 补齐
//! 10/10/10 size。改样本必须先重冻哈希，且不得在看到接受细胞之后改 n。
//! `--pilot-calibrate` 的 decision 永远是 `pilot`，不是门禁 pass/fail。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::analysis::{self, CellRecord, GateReport, Interval, TaskPair};
use crate::harvest;
use crate::suite::{SuitePack, SuiteTask};

pub const PILOT_SCHEMA: &str = "agent-eval.pilot.v1";
pub const PILOT_SALT: &str = "agent-eval.pilot.v1";
/// 与 `analysis` 的 `POWER_SEED` 十进制 ASCII 相同（20260814）。
const PILOT_SEED_ASCII: &[u8] = b"20260814";
pub const PILOT_N: usize = 30;
pub const PILOT_PER_SIZE: usize = 10;

/// 冻结样本 id（按 id 排序）。算法输出必须与此逐字相同。
pub const FROZEN_PILOT_IDS: &[&str] = &[
    "js-ms-minutes-shadow",
    "js-ms-negative-parse",
    "openai-wire-tool-names",
    "python-itertools-batched",
    "python-pep616-removeprefix",
    "python-symbols-add-tests",
    "rust-grep-cooperative-cancel",
    "rust-jcs-canonical-objects",
    "swebench-django__django-11749",
    "swebench-django__django-11999",
    "swebench-django__django-12708",
    "swebench-django__django-13344",
    "swebench-django__django-13809",
    "swebench-django__django-14007",
    "swebench-django__django-14011",
    "swebench-django__django-15268",
    "swebench-django__django-15503",
    "swebench-django__django-15695",
    "swebench-django__django-16642",
    "swebench-matplotlib__matplotlib-23314",
    "swebench-pylint-dev__pylint-4551",
    "swebench-pytest-dev__pytest-5787",
    "swebench-pytest-dev__pytest-6202",
    "swebench-pytest-dev__pytest-7571",
    "swebench-scikit-learn__scikit-learn-11310",
    "swebench-scikit-learn__scikit-learn-13496",
    "swebench-scikit-learn__scikit-learn-14894",
    "swebench-sphinx-doc__sphinx-8548",
    "swebench-sympy__sympy-22914",
    "uuid-parity-keys",
];

/// `sha256(id || "\n" for id in FROZEN_PILOT_IDS)`。
pub const FROZEN_PILOT_SHA256: &str =
    "fa8c5308520bc9b3b51cf0100bc14e78d2c2ca666d06010e27429455e0426431";

#[derive(Debug, Clone)]
pub struct PilotSample {
    pub sha256: String,
    pub tasks: Vec<SuiteTask>,
}

#[derive(Debug, Clone)]
pub struct CalibrationReport {
    pub decision: &'static str,
    pub sample_sha256: String,
    pub expected_n: usize,
    pub observed_n: usize,
    pub missing_ids: Vec<String>,
    pub extra_ids: Vec<String>,
    pub coverage_cells: u32,
    pub intended_cells: u32,
    pub overall_a: f64,
    pub overall_b: f64,
    pub overall_c: f64,
    pub by_size: BTreeMap<String, SizeRates>,
    pub task_corr_ac: Option<f64>,
    pub cell_phi_ac: Option<f64>,
    pub residual_corr_ac: Option<f64>,
    pub mean_a_var_ratio: Option<f64>,
    pub interval: Option<Interval>,
    pub tasks: Vec<TaskPair>,
    pub notes: Vec<String>,
    pub gate: GateReport,
}

#[derive(Debug, Clone)]
pub struct SizeRates {
    pub n_tasks: usize,
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

pub fn sample_sha256(ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(hasher.finalize())
}

fn rank_key(id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PILOT_SALT.as_bytes());
    hasher.update([0]);
    hasher.update(PILOT_SEED_ASCII);
    hasher.update([0]);
    hasher.update(id.as_bytes());
    hasher.finalize().into()
}

/// 从冻结包选出 30 题。与 `FROZEN_PILOT_IDS` 不一致则失败，避免静默漂样本。
pub fn select_pilot(pack: &SuitePack) -> anyhow::Result<PilotSample> {
    let ids = select_pilot_ids(pack);
    if ids.len() != PILOT_N {
        anyhow::bail!("pilot sample n={} (want {PILOT_N})", ids.len());
    }
    if ids != FROZEN_PILOT_IDS {
        anyhow::bail!("pilot sample drifted from the freeze lock; re-register before seeing cells");
    }
    let sha = sample_sha256(&ids);
    if sha != FROZEN_PILOT_SHA256 {
        anyhow::bail!("pilot sample sha256 drifted: {sha}");
    }
    let mut by_id: BTreeMap<&str, &SuiteTask> = BTreeMap::new();
    for task in &pack.tasks {
        by_id.insert(task.id.as_str(), task);
    }
    let mut tasks = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(task) = by_id.get(id.as_str()) else {
            anyhow::bail!("frozen pilot id {id} missing from suite pack");
        };
        tasks.push((*task).clone());
    }
    Ok(PilotSample { sha256: sha, tasks })
}

pub fn select_pilot_ids(pack: &SuitePack) -> Vec<String> {
    let mut file = Vec::new();
    let mut swebench: BTreeMap<&str, Vec<&SuiteTask>> = BTreeMap::new();
    for task in &pack.tasks {
        if task.runtime == harvest::RUNTIME {
            swebench.entry(task.size.as_str()).or_default().push(task);
        } else {
            file.push(task);
        }
    }
    file.sort_by(|left, right| left.id.cmp(&right.id));
    let mut need: BTreeMap<&str, usize> = ["small", "medium", "large"]
        .into_iter()
        .map(|size| (size, PILOT_PER_SIZE))
        .collect();
    let mut selected = Vec::new();
    for task in file {
        selected.push(task.id.clone());
        if let Some(left) = need.get_mut(task.size.as_str()) {
            *left = left.saturating_sub(1);
        }
    }
    for size in ["small", "medium", "large"] {
        let mut cands = swebench.remove(size).unwrap_or_default();
        cands.sort_by(|left, right| {
            rank_key(&left.id)
                .cmp(&rank_key(&right.id))
                .then_with(|| left.id.cmp(&right.id))
        });
        let want = need.get(size).copied().unwrap_or(0);
        for task in cands.into_iter().take(want) {
            selected.push(task.id.clone());
        }
    }
    selected.sort();
    selected
}

pub fn is_file_runtime(task: &SuiteTask) -> bool {
    task.runtime != harvest::RUNTIME
}

pub fn render_pilot(sample: &PilotSample) -> String {
    let mut languages = BTreeSet::new();
    let mut classes = BTreeSet::new();
    let mut sizes: BTreeMap<&str, u32> = BTreeMap::new();
    let mut n_file = 0u32;
    let mut n_docker = 0u32;
    for task in &sample.tasks {
        languages.insert(task.language.as_str());
        classes.insert(task.class.as_str());
        *sizes.entry(task.size.as_str()).or_default() += 1;
        if is_file_runtime(task) {
            n_file += 1;
        } else {
            n_docker += 1;
        }
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{PILOT_SCHEMA} frozen=true n={}/{PILOT_N} sha256={}\n",
        sample.tasks.len(),
        sample.sha256
    ));
    out.push_str(
        "non-acceptance calibration sample; --pilot-calibrate decision=pilot; not the 300x3 gate\n",
    );
    out.push_str(&format!(
        "mix file={n_file} swebench-docker={n_docker} languages={} classes={} sizes small/medium/large={}/{}/{}\n",
        languages.len(),
        classes.len(),
        sizes.get("small").copied().unwrap_or(0),
        sizes.get("medium").copied().unwrap_or(0),
        sizes.get("large").copied().unwrap_or(0),
    ));
    out.push_str(
        "default --pilot-run is file-only (9 tasks). --include-swebench needs AGENT_EVAL_SWEBENCH_CLONE=1 and AGENT_EVAL_SWEBENCH_DOCKER=1.\n",
    );
    for task in &sample.tasks {
        out.push_str(&format!(
            "  {} size={} class={} lang={} runtime={}\n",
            task.id,
            task.size,
            task.class,
            task.language,
            if task.runtime.is_empty() {
                "files"
            } else {
                task.runtime.as_str()
            }
        ));
    }
    out
}

/// 对照功效模型：三分层 p=0.90/0.70/0.40，A ⟂ C | task。永不输出 pass/fail。
pub fn calibrate(
    cells: &[CellRecord],
    expected_ids: &[&str],
    sizes: &BTreeMap<String, String>,
) -> CalibrationReport {
    let gate = analysis::analyze(cells);
    let observed: BTreeSet<&str> = gate
        .tasks
        .iter()
        .map(|task| task.fixture_id.as_str())
        .collect();
    let expected: BTreeSet<&str> = expected_ids.iter().copied().collect();
    let missing_ids: Vec<String> = expected
        .difference(&observed)
        .map(|id| (*id).to_string())
        .collect();
    let extra_ids: Vec<String> = observed
        .difference(&expected)
        .map(|id| (*id).to_string())
        .collect();

    let mut notes = Vec::new();
    notes.push("decision=pilot; never treat this as the 300x3 gate".into());
    notes.push(
        "amend n only by EVAL-01.3 re-registration, never after seeing acceptance cells".into(),
    );
    if !missing_ids.is_empty() {
        notes.push(format!(
            "incomplete sample: missing {}/{} tasks (file-only runs skip swebench-docker)",
            missing_ids.len(),
            expected_ids.len()
        ));
    }
    if !extra_ids.is_empty() {
        notes.push(format!(
            "extra ids outside the frozen sample: {}",
            extra_ids.join(",")
        ));
    }
    if gate.eligible {
        notes.push(
            "BUG: analyze() marked a pilot cell set eligible; the 300-task gate must stay closed"
                .into(),
        );
    }

    let intended_cells = (expected_ids.len() as u32)
        .saturating_mul(analysis::GATE_REPEATS)
        .saturating_mul(3);
    let coverage_cells = cells.len() as u32;

    let (overall_a, overall_b, overall_c) = overall_rates(cells.iter());
    let by_size = rates_by_size(cells, sizes);
    let task_corr_ac = pearson(
        &gate
            .tasks
            .iter()
            .map(|task| task.a_rate)
            .collect::<Vec<_>>(),
        &gate
            .tasks
            .iter()
            .map(|task| task.c_rate)
            .collect::<Vec<_>>(),
    );
    let cell_phi_ac = phi_ac(cells);
    let residual_corr_ac = task_residual_corr_ac(cells);
    let mean_a_var_ratio = mean_bernoulli_var_ratio(cells);
    let diffs: Vec<f64> = gate.tasks.iter().map(|task| task.diff).collect();
    let interval = analysis::one_sided_lcl(&diffs);

    if let Some(ratio) = mean_a_var_ratio {
        notes.push(format!(
            "mean within-task A variance / Bernoulli p(1-p) = {ratio:.3} (1 ≈ power-model iid repeats)"
        ));
    }
    if let Some(corr) = task_corr_ac {
        notes.push(format!(
            "task-level corr(A_rate, C_rate) = {corr:.3} (shared difficulty p predicts positive)"
        ));
    }
    if let Some(phi) = cell_phi_ac {
        notes.push(format!(
            "repeat-level pooled phi(A, C) = {phi:.3} (confounded by task difficulty; not a test of A ⟂ C | task)"
        ));
    }
    if let Some(corr) = residual_corr_ac {
        notes.push(format!(
            "task-residual corr(A, C) = {corr:.3} (A_i - task A rate vs C_i - task C rate; A ⟂ C | task predicts ~0)"
        ));
    } else {
        notes.push(
            "task-residual corr(A, C) undefined (zero residual variance; pooled phi cannot reject A ⟂ C | task)"
                .into(),
        );
    }
    notes.push(
        "power-model strata p: small=0.90 medium=0.70 large=0.40; observed A by size is diagnostic only"
            .to_string(),
    );

    CalibrationReport {
        decision: "pilot",
        sample_sha256: FROZEN_PILOT_SHA256.to_string(),
        expected_n: expected_ids.len(),
        observed_n: gate.tasks.len(),
        missing_ids,
        extra_ids,
        coverage_cells,
        intended_cells,
        overall_a,
        overall_b,
        overall_c,
        by_size,
        task_corr_ac,
        cell_phi_ac,
        residual_corr_ac,
        mean_a_var_ratio,
        interval,
        tasks: gate.tasks.clone(),
        notes,
        gate,
    }
}

pub fn render_calibration(report: &CalibrationReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{PILOT_SCHEMA} decision={} sample_sha256={}\n",
        report.decision, report.sample_sha256
    ));
    out.push_str(&format!(
        "coverage tasks={}/{} cells={}/{} missing={} extra={}\n",
        report.observed_n,
        report.expected_n,
        report.coverage_cells,
        report.intended_cells,
        report.missing_ids.len(),
        report.extra_ids.len()
    ));
    out.push_str(&format!(
        "ITT rates A={:.3} B={:.3} C={:.3}\n",
        report.overall_a, report.overall_b, report.overall_c
    ));
    for size in ["small", "medium", "large"] {
        if let Some(row) = report.by_size.get(size) {
            let expected = match size {
                "small" => 0.90,
                "medium" => 0.70,
                _ => 0.40,
            };
            out.push_str(&format!(
                "  size {size} n={} A={:.3} (model {expected:.2}) B={:.3} C={:.3}\n",
                row.n_tasks, row.a, row.b, row.c
            ));
        }
    }
    if let Some(interval) = &report.interval {
        out.push_str(&format!(
            "diagnostic C-A: n_tasks={} mean={:.6} se={:.6} LCL={:.6} (not a gate)\n",
            interval.n_tasks, interval.mean, interval.se, interval.lcl
        ));
    }
    if let Some(corr) = report.task_corr_ac {
        out.push_str(&format!(
            "task-level corr(A,C)={corr:.3} (shared p predicts +)\n"
        ));
    }
    if let Some(phi) = report.cell_phi_ac {
        out.push_str(&format!(
            "repeat-level pooled phi(A,C)={phi:.3} (confounded by task difficulty; not A indep C | task)\n"
        ));
    }
    if let Some(corr) = report.residual_corr_ac {
        out.push_str(&format!(
            "task-residual corr(A,C)={corr:.3} (A_i - task rate vs C_i - task rate; A indep C | task predicts ~0)\n"
        ));
    } else {
        out.push_str("task-residual corr(A,C)=undefined (zero residual variance)\n");
    }
    if let Some(ratio) = report.mean_a_var_ratio {
        out.push_str(&format!(
            "mean A var / Bernoulli p(1-p)={ratio:.3} (1 ≈ iid repeats)\n"
        ));
    }
    out.push_str(&format!("paired tasks listed={}\n", report.tasks.len()));
    for note in &report.notes {
        out.push_str(&format!("  note: {note}\n"));
    }
    for id in &report.missing_ids {
        out.push_str(&format!("  missing: {id}\n"));
    }
    out.push_str(&analysis::render_report(&report.gate));
    out
}

pub fn size_index(pack: &SuitePack) -> BTreeMap<String, String> {
    pack.tasks
        .iter()
        .map(|task| (task.id.clone(), task.size.clone()))
        .collect()
}

pub fn load_and_calibrate(root: &Path, file_only: bool) -> anyhow::Result<CalibrationReport> {
    let pack = crate::suite::load_pack()?;
    let mut cells = analysis::load_evidence_root(root)?;
    if file_only {
        cells.retain(|cell| cell_is_file_runtime(cell, &pack));
    }
    Ok(calibrate(&cells, FROZEN_PILOT_IDS, &size_index(&pack)))
}

fn cell_is_file_runtime(cell: &CellRecord, pack: &SuitePack) -> bool {
    pack.tasks
        .iter()
        .find(|task| task.id == cell.fixture_id)
        .map(is_file_runtime)
        .unwrap_or(true)
}

fn overall_rates<'a>(cells: impl IntoIterator<Item = &'a CellRecord>) -> (f64, f64, f64) {
    let mut a = (0u32, 0u32);
    let mut b = (0u32, 0u32);
    let mut c = (0u32, 0u32);
    for cell in cells {
        match cell.engine.as_str() {
            "append" => {
                a.1 += 1;
                if cell.itt_success() {
                    a.0 += 1;
                }
            }
            "rolling" => {
                b.1 += 1;
                if cell.itt_success() {
                    b.0 += 1;
                }
            }
            "dynamic" => {
                c.1 += 1;
                if cell.itt_success() {
                    c.0 += 1;
                }
            }
            _ => {}
        }
    }
    (rate(a), rate(b), rate(c))
}

fn rates_by_size(
    cells: &[CellRecord],
    sizes: &BTreeMap<String, String>,
) -> BTreeMap<String, SizeRates> {
    let mut grouped: BTreeMap<String, Vec<&CellRecord>> = BTreeMap::new();
    for cell in cells {
        let size = sizes
            .get(&cell.fixture_id)
            .cloned()
            .unwrap_or_else(|| "unknown".into());
        grouped.entry(size).or_default().push(cell);
    }
    grouped
        .into_iter()
        .map(|(size, rows)| {
            let (a, b, c) = overall_rates(rows.iter().copied());
            let n_tasks = rows
                .iter()
                .map(|cell| cell.fixture_id.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            (size, SizeRates { n_tasks, a, b, c })
        })
        .collect()
}

fn rate(pair: (u32, u32)) -> f64 {
    if pair.1 == 0 {
        0.0
    } else {
        pair.0 as f64 / pair.1 as f64
    }
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 3 || xs.len() != ys.len() {
        return None;
    }
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut cov = 0.0;
    let mut vx = 0.0;
    let mut vy = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        let dx = x - mx;
        let dy = y - my;
        cov += dx * dy;
        vx += dx * dx;
        vy += dy * dy;
    }
    if vx <= 1e-15 || vy <= 1e-15 {
        return None;
    }
    Some(cov / (vx.sqrt() * vy.sqrt()))
}

fn phi_ac(cells: &[CellRecord]) -> Option<f64> {
    let mut by_key: BTreeMap<(String, u32), (Option<bool>, Option<bool>)> = BTreeMap::new();
    for cell in cells {
        let entry = by_key
            .entry((cell.fixture_id.clone(), cell.repeat))
            .or_insert((None, None));
        if cell.engine == "append" {
            entry.0 = Some(cell.itt_success());
        } else if cell.engine == "dynamic" {
            entry.1 = Some(cell.itt_success());
        }
    }
    let mut n11 = 0.0;
    let mut n10 = 0.0;
    let mut n01 = 0.0;
    let mut n00 = 0.0;
    let mut n = 0.0;
    for (a, c) in by_key.values() {
        let (Some(a), Some(c)) = (a, c) else {
            continue;
        };
        n += 1.0;
        match (a, c) {
            (true, true) => n11 += 1.0,
            (true, false) => n10 += 1.0,
            (false, true) => n01 += 1.0,
            (false, false) => n00 += 1.0,
        }
    }
    if n < 3.0 {
        return None;
    }
    let prod: f64 = (n11 + n10) * (n01 + n00) * (n11 + n01) * (n10 + n00);
    if prod <= 1e-15 {
        return None;
    }
    Some((n11 * n00 - n10 * n01) / prod.sqrt())
}

fn task_residual_corr_ac(cells: &[CellRecord]) -> Option<f64> {
    let mut by_key: BTreeMap<(String, u32), (Option<bool>, Option<bool>)> = BTreeMap::new();
    for cell in cells {
        let entry = by_key
            .entry((cell.fixture_id.clone(), cell.repeat))
            .or_insert((None, None));
        if cell.engine == "append" {
            entry.0 = Some(cell.itt_success());
        } else if cell.engine == "dynamic" {
            entry.1 = Some(cell.itt_success());
        }
    }
    let mut pairs: Vec<(String, f64, f64)> = Vec::new();
    for ((fixture, _), (a, c)) in &by_key {
        let (Some(a), Some(c)) = (a, c) else {
            continue;
        };
        pairs.push((
            fixture.clone(),
            if *a { 1.0 } else { 0.0 },
            if *c { 1.0 } else { 0.0 },
        ));
    }
    if pairs.len() < 3 {
        return None;
    }
    let mut sums: BTreeMap<&str, (f64, f64, f64)> = BTreeMap::new();
    for (fixture, a, c) in &pairs {
        let entry = sums.entry(fixture.as_str()).or_insert((0.0, 0.0, 0.0));
        entry.0 += *a;
        entry.1 += *c;
        entry.2 += 1.0;
    }
    let mut xs = Vec::with_capacity(pairs.len());
    let mut ys = Vec::with_capacity(pairs.len());
    for (fixture, a, c) in &pairs {
        let (sa, sc, n) = sums.get(fixture.as_str())?;
        xs.push(*a - *sa / *n);
        ys.push(*c - *sc / *n);
    }
    pearson(&xs, &ys)
}

fn mean_bernoulli_var_ratio(cells: &[CellRecord]) -> Option<f64> {
    let mut by_task: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for cell in cells {
        if cell.engine != "append" {
            continue;
        }
        by_task
            .entry(cell.fixture_id.clone())
            .or_default()
            .push(if cell.itt_success() { 1.0 } else { 0.0 });
    }
    let mut ratios = Vec::new();
    for trials in by_task.values() {
        if trials.len() < 2 {
            continue;
        }
        let p = trials.iter().sum::<f64>() / trials.len() as f64;
        let expected = p * (1.0 - p);
        if expected <= 1e-15 {
            continue;
        }
        let mean = p;
        let var = trials.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>()
            / (trials.len() as f64 - 1.0);
        ratios.push(var / expected);
    }
    if ratios.is_empty() {
        None
    } else {
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::CellRecord;

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
            model_input_tokens: 10,
            rounds: 1,
            tool_calls: 1,
            missing: false,
            search_calls: 0,
            search_hits: 0,
            search_empty: 0,
            search_ms_p50: 0,
            forgotten_items: 0,
            recovered_items: 0,
            access_search_hits: 0,
            access_inspects: 0,
            access_fetches: 0,
            access_consumption_acks: 0,
        }
    }

    #[test]
    fn frozen_sample_matches_pack_and_hash() {
        let pack = crate::suite::load_pack().expect("suite pack");
        let computed = select_pilot_ids(&pack);
        assert_eq!(computed, FROZEN_PILOT_IDS);
        assert_eq!(sample_sha256(&computed), FROZEN_PILOT_SHA256);
        let sample = select_pilot(&pack).expect("select");
        assert_eq!(sample.tasks.len(), PILOT_N);
        let mut sizes: BTreeMap<&str, u32> = BTreeMap::new();
        let mut n_file = 0;
        for task in &sample.tasks {
            *sizes.entry(task.size.as_str()).or_default() += 1;
            if is_file_runtime(task) {
                n_file += 1;
            }
        }
        assert_eq!(n_file, 9);
        assert_eq!(sizes.get("small").copied(), Some(10));
        assert_eq!(sizes.get("medium").copied(), Some(10));
        assert_eq!(sizes.get("large").copied(), Some(10));
        let rendered = render_pilot(&sample);
        assert!(rendered.contains("decision=pilot"));
        assert!(rendered.contains(FROZEN_PILOT_SHA256));
        assert!(rendered.contains("python-itertools-batched"));
        assert!(rendered.contains("swebench-django__django-11749"));
    }

    #[test]
    fn calibration_never_opens_the_gate() {
        let pack = crate::suite::load_pack().unwrap();
        let mut cells = Vec::new();
        for id in FROZEN_PILOT_IDS {
            for repeat in 1..=3 {
                cells.push(cell(id, repeat, "append", true));
                cells.push(cell(id, repeat, "rolling", true));
                cells.push(cell(id, repeat, "dynamic", true));
            }
        }
        let report = calibrate(&cells, FROZEN_PILOT_IDS, &size_index(&pack));
        assert_eq!(report.decision, "pilot");
        assert!(!report.gate.eligible);
        assert_eq!(report.gate.decision, "ineligible");
        assert_eq!(report.observed_n, 30);
        assert!(report.missing_ids.is_empty());
        assert!((report.overall_a - 1.0).abs() < 1e-12);
        let text = render_calibration(&report);
        assert!(text.contains("decision=pilot"));
        assert!(text.contains("n_tasks=30 != 300"));
        assert!(!text.contains("decision=pass\n"));
    }

    #[test]
    fn pooled_phi_is_confounded_by_task_difficulty() {
        let mut cells = Vec::new();
        for repeat in 1..=3 {
            cells.push(cell("easy", repeat, "append", true));
            cells.push(cell("easy", repeat, "dynamic", true));
            cells.push(cell("hard", repeat, "append", false));
            cells.push(cell("hard", repeat, "dynamic", false));
        }
        let phi = phi_ac(&cells).unwrap();
        assert!(
            phi > 0.99,
            "unconditional phi should be ~1 when tasks share difficulty: {phi}"
        );
        assert!(
            task_residual_corr_ac(&cells).is_none(),
            "zero within-task residual variance is undefined, not evidence against A ⟂ C | task"
        );
    }

    #[test]
    fn file_only_cells_are_incomplete_not_acceptance() {
        let pack = crate::suite::load_pack().unwrap();
        let file_ids: Vec<&str> = FROZEN_PILOT_IDS
            .iter()
            .copied()
            .filter(|id| !id.starts_with("swebench-"))
            .collect();
        let mut cells = Vec::new();
        for id in &file_ids {
            cells.push(cell(id, 1, "append", true));
            cells.push(cell(id, 1, "rolling", false));
            cells.push(cell(id, 1, "dynamic", true));
        }
        let report = calibrate(&cells, FROZEN_PILOT_IDS, &size_index(&pack));
        assert_eq!(report.decision, "pilot");
        assert_eq!(report.observed_n, 9);
        assert_eq!(report.missing_ids.len(), 21);
        assert!(!report.gate.eligible);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("incomplete sample"))
        );
    }

    #[test]
    fn file_only_filter_drops_swebench_floor_cells() {
        let pack = crate::suite::load_pack().unwrap();
        let file_id = "js-ms-negative-parse";
        let swebench_id = "swebench-django__django-11749";
        let mut cells = Vec::new();
        for repeat in 1..=3 {
            cells.push(cell(file_id, repeat, "append", true));
            cells.push(cell(file_id, repeat, "rolling", true));
            cells.push(cell(file_id, repeat, "dynamic", true));
            cells.push(cell(swebench_id, repeat, "append", false));
            cells.push(cell(swebench_id, repeat, "rolling", false));
            cells.push(cell(swebench_id, repeat, "dynamic", false));
        }
        cells.retain(|cell| cell_is_file_runtime(cell, &pack));
        let report = calibrate(&cells, FROZEN_PILOT_IDS, &size_index(&pack));
        assert_eq!(report.observed_n, 1);
        assert!((report.overall_a - 1.0).abs() < 1e-12);
        assert!((report.overall_c - 1.0).abs() < 1e-12);
        assert!(
            report
                .tasks
                .iter()
                .all(|task| !task.fixture_id.starts_with("swebench-"))
        );
    }
}

//! EVAL-02 Context Benchmark: the current M15 decision instrument.
//!
//! Asks where the dynamic context runtime helps or hurts a coding agent.
//! Independent of `agent-eval.analysis.v2` (300×3 ITT stays frozen and parked).
//! SPEC and pack digest are hash-frozen; changing either fails CI.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::suite::{self, SuiteHiddenCommand};
use crate::workload::{self, HiddenAssertionResult, HiddenFileBody, HiddenReport, VERIFY_SCHEMA};

pub const SCHEMA: &str = "agent-eval.context-bench.v1";
pub const WAVE1_TASKS: usize = 12;
pub const WAVE1_AC_CELLS: usize = 24;
pub const WAVE1_ROLLING_CELLS: usize = 3;
pub const WAVE1_CELLS: usize = WAVE1_AC_CELLS + WAVE1_ROLLING_CELLS;

/// Rolling is an attribution baseline, not a primary contrast.
pub const ROLLING_TASKS: [&str; 3] = ["horizon_long", "semantic_recall", "task_switch"];

/// Pre-registered text. Changing any byte changes `spec_sha256`.
pub const SPEC: &str = "\
schema=agent-eval.context-bench.v1
question=where does the dynamic context runtime help or hurt a coding agent
primary=per-scenario why report; not a success-rate LCL gate
engines=append vs dynamic on all 12 tasks; rolling only on horizon_long, semantic_recall, task_switch
wave1=12 tasks x A/C x 1 repeat = 24 cells; plus 3 rolling cells = 27
repeats=1; discordant or anomalous cells may add a second repeat
horizon_medium=deferred
hidden=file asserts plus optional out-of-workspace commands; constraints are not planted in seed
resume_point=not implemented; task_switch measures whether suspend/activate is enough
scoring=frozen; do not retune from this bench
analysis_v2=untouched; 300x3 ITT parked until this bench says C is worth continuing
live_rounds=48 shared A/B/C; length comes from staged user turns, not a higher C cap
evidence_identity=task_sha256 covers json+seed+golden+checker; pack_digest covers pack.json+spec+tasks
provider_tokens=coding_in+coding_out+compactor_in+compactor_out
report=delta abs and pct; actual rounds; peak and final resident
hidden_live=missing verifier is preflight fail
frozen=true
";

/// Filled after the deterministic pack self-check is green.
pub const FROZEN_SPEC_SHA256: &str =
    "12dc8e22f3a649b619f719f4a18e0cf73486a668aded4912ca93a469b22bc902";
pub const FROZEN_PACK_DIGEST: &str =
    "00a6079ee601cd0004060acb168603c80d5d77dc62e77caf1782eccd88e2d38e";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizationTarget {
    Search,
    EpisodeCompaction,
    Gc,
    TaskAnchor,
    ToolFailure,
    None,
}

impl OptimizationTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::EpisodeCompaction => "Episode compaction",
            Self::Gc => "GC",
            Self::TaskAnchor => "TaskAnchor / ResumePoint",
            Self::ToolFailure => "Tool failure",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BenchTaskFile {
    pub id: String,
    pub scenario: String,
    pub variant: String,
    pub name: String,
    pub seed: String,
    #[serde(default)]
    pub include_rolling: bool,
    pub target_rounds_lo: u32,
    pub target_rounds_hi: u32,
    pub expected_edit: String,
    pub ops: Vec<TurnOp>,
    #[serde(default)]
    pub hidden: Vec<BenchHidden>,
    #[serde(default)]
    pub hidden_commands: Vec<BenchHiddenCommand>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TurnOp {
    User {
        text: String,
    },
    Suspend,
    Activate {
        slot: String,
    },
    Complete {
        #[serde(default)]
        summary: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct BenchHidden {
    pub path: String,
    pub pred: String,
    #[serde(default)]
    pub needles: Vec<String>,
    #[serde(default)]
    pub min: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BenchHiddenCommand {
    pub name: String,
    pub script: String,
}

#[derive(Debug, Clone)]
pub struct BenchTask {
    pub file: BenchTaskFile,
    pub path: PathBuf,
}

impl BenchTask {
    pub fn id(&self) -> &str {
        &self.file.id
    }

    pub fn include_rolling(&self) -> bool {
        self.file.include_rolling
    }

    pub fn engines(&self) -> Vec<&'static str> {
        if self.include_rolling() {
            vec!["append", "rolling", "dynamic"]
        } else {
            vec!["append", "dynamic"]
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchPack {
    pub root: PathBuf,
    pub tasks: Vec<BenchTask>,
}

impl BenchPack {
    pub fn task(&self, id: &str) -> Option<&BenchTask> {
        self.tasks.iter().find(|task| task.id() == id)
    }
}

pub fn bench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("context-bench")
}

pub fn load_pack() -> anyhow::Result<BenchPack> {
    load_pack_from(&bench_root())
}

pub fn load_pack_from(root: &Path) -> anyhow::Result<BenchPack> {
    let pack_path = root.join("pack.json");
    let pack: PackFile = serde_json::from_str(
        &fs::read_to_string(&pack_path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", pack_path.display()))?,
    )?;
    if pack.schema != SCHEMA {
        anyhow::bail!("context-bench pack schema {} != {SCHEMA}", pack.schema);
    }
    let mut tasks = Vec::new();
    for rel in &pack.tasks {
        let path = root.join(rel);
        let file: BenchTaskFile = serde_json::from_str(
            &fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?,
        )?;
        tasks.push(BenchTask { file, path });
    }
    if tasks.len() != WAVE1_TASKS {
        anyhow::bail!(
            "context-bench pack has {} tasks, expected {WAVE1_TASKS}",
            tasks.len()
        );
    }
    Ok(BenchPack {
        root: root.to_path_buf(),
        tasks,
    })
}

#[derive(Debug, Deserialize)]
struct PackFile {
    schema: String,
    tasks: Vec<String>,
}

pub fn rolling_task_ids() -> &'static [&'static str] {
    &ROLLING_TASKS
}

pub fn wave1_cell_count(pack: &BenchPack) -> usize {
    pack.tasks.iter().map(|task| task.engines().len()).sum()
}

pub fn render_pack(pack: &BenchPack) -> String {
    let mut out = String::new();
    out.push_str(&format!("schema={SCHEMA}\n"));
    out.push_str(&format!("spec_sha256={}\n", spec_sha256()));
    out.push_str(&format!("pack_digest={}\n", pack_digest(pack)));
    out.push_str("decision_instrument=context-bench (not 300x3 ITT, not the 30-task pilot)\n");
    out.push_str(&format!(
        "wave1_cells={} ({} A/C + {} rolling)\n",
        wave1_cell_count(pack),
        WAVE1_AC_CELLS,
        ROLLING_TASKS.len()
    ));
    out.push_str("repeats=1\n");
    out.push_str("spec:\n");
    for line in SPEC.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("tasks:\n");
    for task in &pack.tasks {
        out.push_str(&format!(
            "  {:<28} scenario={:<16} variant={:<12} rolling={} rounds={}..{} ops={}  {}\n",
            task.id(),
            task.file.scenario,
            task.file.variant,
            task.include_rolling(),
            task.file.target_rounds_lo,
            task.file.target_rounds_hi,
            task.file.ops.len(),
            task.file.name
        ));
    }
    out
}

pub fn seed_task(pack: &BenchPack, task: &BenchTask, dest: &Path) -> anyhow::Result<()> {
    let seed = pack.root.join("seeds").join(&task.file.seed);
    if !seed.is_dir() {
        anyhow::bail!("missing seed dir {}", seed.display());
    }
    copy_dir(&seed, dest)?;
    Ok(())
}

pub fn apply_golden(pack: &BenchPack, task: &BenchTask, dest: &Path) -> anyhow::Result<()> {
    let golden = pack.root.join("golden").join(task.id());
    if !golden.is_dir() {
        anyhow::bail!("missing golden dir {}", golden.display());
    }
    copy_dir(&golden, dest)?;
    Ok(())
}

pub fn evaluate_task(pack: &BenchPack, task: &BenchTask, root: &Path) -> HiddenReport {
    let mut bodies: BTreeMap<String, HiddenFileBody> = BTreeMap::new();
    for assert in &task.file.hidden {
        bodies
            .entry(assert.path.clone())
            .or_insert_with(|| workload::read_hidden_file(root, &assert.path));
    }
    let assertions: Vec<HiddenAssertionResult> = task
        .file
        .hidden
        .iter()
        .map(|assert| {
            let file = bodies
                .get(&assert.path)
                .expect("every hidden path is collected");
            let needles: Vec<&str> = assert.needles.iter().map(String::as_str).collect();
            let (passed, count) =
                workload::eval_pred(&file.body, &assert.pred, &needles, assert.min);
            HiddenAssertionResult {
                path: assert.path.clone(),
                pred: assert.pred.clone(),
                needles: assert.needles.clone(),
                min: assert.min,
                count: Some(count),
                passed,
                file_exists: file.exists,
            }
        })
        .collect();
    let mut commands = Vec::new();
    for command in &task.file.hidden_commands {
        let script = pack.root.join("checks").join(&command.script);
        let spec = SuiteHiddenCommand {
            argv: vec![
                "python".into(),
                script.to_string_lossy().into_owned(),
                root.to_string_lossy().into_owned(),
            ],
            timeout_ms: 15_000,
            expect_exit: 0,
        };
        let mut result = suite::run_hidden_command(root, &spec);
        if !result.passed && result.stderr.is_empty() {
            result.stderr = format!("{} failed", command.name);
        } else if !result.passed {
            result.stderr = format!("{}: {}", command.name, result.stderr);
        }
        commands.push(result);
    }
    let files_pass = assertions.iter().all(|row| row.passed);
    let commands_pass = commands.iter().all(|row| row.passed);
    HiddenReport {
        schema: VERIFY_SCHEMA.to_string(),
        kind: if commands.is_empty() {
            "file_content".into()
        } else {
            "file_content+command".into()
        },
        fixture_id: task.id().to_string(),
        expected_edit: task.file.expected_edit.clone(),
        passed: files_pass && commands_pass,
        replay_complete: bodies.values().all(|file| !file.truncated),
        assertions,
        files: bodies.into_values().collect(),
        commands,
    }
}

pub fn spec_sha256() -> String {
    hex_encode(Sha256::digest(SPEC.as_bytes()))
}

pub fn task_sha256(pack: &BenchPack, task: &BenchTask) -> String {
    let mut hasher = Sha256::new();
    hasher.update(task.id().as_bytes());
    hasher.update([0]);
    hash_named(&mut hasher, "task.json", &task.path);
    hash_tree(
        &mut hasher,
        "seed",
        &pack.root.join("seeds").join(&task.file.seed),
    );
    hash_tree(
        &mut hasher,
        "golden",
        &pack.root.join("golden").join(task.id()),
    );
    for command in &task.file.hidden_commands {
        hash_named(
            &mut hasher,
            &format!("checks/{}", command.script),
            &pack.root.join("checks").join(&command.script),
        );
    }
    hex_encode(hasher.finalize())
}

pub fn pack_digest(pack: &BenchPack) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA.as_bytes());
    hasher.update([0]);
    hasher.update(SPEC.as_bytes());
    hasher.update([0]);
    hash_named(&mut hasher, "pack.json", &pack.root.join("pack.json"));
    for task in &pack.tasks {
        hasher.update(task.id().as_bytes());
        hasher.update([0]);
        hasher.update(task_sha256(pack, task).as_bytes());
        hasher.update([0]);
    }
    hex_encode(hasher.finalize())
}

pub fn require_python() -> anyhow::Result<()> {
    let bin = crate::harvest::python_bin();
    match std::process::Command::new(&bin)
        .arg("-c")
        .arg("import sys")
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            anyhow::bail!("context-bench verifier preflight failed: {bin} exited {status}")
        }
        Err(error) => {
            anyhow::bail!("context-bench verifier preflight failed: {bin} missing ({error})")
        }
    }
}

pub fn check_pack(pack: &BenchPack) -> anyhow::Result<String> {
    require_python()?;
    if spec_sha256() != FROZEN_SPEC_SHA256 {
        anyhow::bail!("SPEC hash {} != frozen {FROZEN_SPEC_SHA256}", spec_sha256());
    }
    let digest = pack_digest(pack);
    if digest != FROZEN_PACK_DIGEST {
        anyhow::bail!("pack digest {digest} != frozen {FROZEN_PACK_DIGEST}");
    }
    let mut out = String::new();
    if wave1_cell_count(pack) != WAVE1_CELLS {
        anyhow::bail!(
            "wave-1 cell count {} != {WAVE1_CELLS}",
            wave1_cell_count(pack)
        );
    }
    for id in rolling_task_ids() {
        let task = pack
            .task(id)
            .ok_or_else(|| anyhow::anyhow!("missing rolling task {id}"))?;
        if !task.include_rolling() {
            anyhow::bail!("{id} must include the rolling arm");
        }
    }
    for task in &pack.tasks {
        let dir = tempfile::tempdir()?;
        seed_task(pack, task, dir.path())?;
        let seeded = evaluate_task(pack, task, dir.path());
        if seeded.assertions.iter().all(|row| row.passed) && !task.file.hidden.is_empty() {
            anyhow::bail!(
                "{} passes file asserts on the seed — the hidden check does not test the change",
                task.id()
            );
        }
        apply_golden(pack, task, dir.path())?;
        let gold = evaluate_task(pack, task, dir.path());
        if !gold.assertions.iter().all(|row| row.passed) {
            let misses: Vec<_> = gold
                .assertions
                .iter()
                .filter(|row| !row.passed)
                .map(|row| format!("{} {}", row.path, row.pred))
                .collect();
            anyhow::bail!(
                "{} golden failed file asserts: {}",
                task.id(),
                misses.join(", ")
            );
        }
        if let Some(failed) = gold.commands.iter().find(|row| !row.passed) {
            anyhow::bail!(
                "{} golden hidden command failed exit={:?} stderr={}",
                task.id(),
                failed.exit,
                failed.stderr
            );
        }
        if !seeded.commands.is_empty() && seeded.commands.iter().all(|row| row.passed) {
            anyhow::bail!(
                "{} hidden commands pass on the seed — the command does not test the change",
                task.id()
            );
        }
        out.push_str(&format!("ok {}\n", task.id()));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct CellView {
    pub engine: String,
    pub passed: bool,
    pub input: u64,
    pub output: u64,
    pub rounds: u64,
    pub resident: u64,
    pub peak_resident: u64,
    pub forgotten: u64,
    pub recovered: u64,
    pub recovery_search: u64,
    pub recovery_reactivate: u64,
    pub recovery_reread: u64,
    pub recovery_failed: u64,
    pub compaction_in: u64,
    pub compaction_out: u64,
    pub search_calls: u64,
    pub search_empty: u64,
    pub failed_tools: u64,
}

impl CellView {
    pub fn from_summary(engine: &str, passed: bool, metrics: &serde_json::Value) -> Self {
        let u = |key: &str| metrics.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        Self {
            engine: engine.to_string(),
            passed,
            input: u("model_input_tokens"),
            output: u("model_output_tokens"),
            rounds: u("rounds"),
            resident: u("final_resident_bytes"),
            peak_resident: u("peak_resident_bytes"),
            forgotten: u("forgotten_items"),
            recovered: u("recovered_items"),
            recovery_search: u("recovery_explicit_search"),
            recovery_reactivate: u("recovery_auto_reactivation"),
            recovery_reread: u("recovery_workspace_reread"),
            recovery_failed: u("recovery_failed"),
            compaction_in: u("compaction_input_tokens"),
            compaction_out: u("compaction_output_tokens"),
            search_calls: u("search_calls"),
            search_empty: u("search_empty"),
            failed_tools: u("failed_tool_outputs"),
        }
    }

    pub fn provider_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.compaction_in)
            .saturating_add(self.compaction_out)
    }
}

pub fn likely_optimization_target(
    scenario: &str,
    a: &CellView,
    c: &CellView,
) -> OptimizationTarget {
    if scenario == "task_switch" {
        if !c.passed && a.passed {
            return OptimizationTarget::TaskAnchor;
        }
        if c.recovery_reread > a.recovery_reread.saturating_mul(2).max(4) {
            return OptimizationTarget::TaskAnchor;
        }
    }
    if scenario == "noise" {
        if !c.passed && c.forgotten > 0 && c.recovery_failed > 0 {
            return OptimizationTarget::ToolFailure;
        }
        if a.passed
            && c.passed
            && a.resident > 0
            && c.resident.saturating_mul(2) > a.resident
            && c.failed_tools + 1 < a.failed_tools
        {
            return OptimizationTarget::ToolFailure;
        }
    }
    if !c.passed
        && a.passed
        && c.forgotten >= 3
        && c.search_calls > 0
        && c.search_empty + 1 >= c.search_calls
    {
        return OptimizationTarget::Search;
    }
    if a.passed && c.passed {
        let token_drop = a.input.saturating_sub(c.input);
        let resident_drop = a.resident.saturating_sub(c.resident);
        if a.input > 0
            && token_drop * 10 < a.input
            && a.resident > 0
            && resident_drop * 5 < a.resident
            && c.compaction_out > 0
        {
            return OptimizationTarget::EpisodeCompaction;
        }
        if a.input > 0
            && token_drop * 5 >= a.input
            && c.recovery_reactivate > 0
            && c.recovery_reactivate >= c.recovery_search
        {
            return OptimizationTarget::Gc;
        }
    }
    OptimizationTarget::None
}

pub fn render_why(task_id: &str, scenario: &str, cells: &[CellView]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Scenario: {task_id}\n"));
    for cell in cells {
        out.push_str(&format!(
            "\n{}\n  success         {}\n  coding          {}/{}\n  compactor       {}/{}\n  provider total  {}\n  actual rounds   {}\n  resident        final={} peak={}\n  forgotten       {}\n  recovered       {}\n",
            cell.engine,
            if cell.passed { "yes" } else { "no" },
            cell.input,
            cell.output,
            cell.compaction_in,
            cell.compaction_out,
            cell.provider_tokens(),
            cell.rounds,
            cell.resident,
            cell.peak_resident,
            cell.forgotten,
            cell.recovered,
        ));
        if cell.engine == "dynamic" || cell.engine == "rolling" {
            out.push_str(&format!(
                "  recovery attribution\n    explicit search       {}\n    auto reactivation     {}\n    workspace rereads     {}\n    failed recoveries     {}\n",
                cell.recovery_search,
                cell.recovery_reactivate,
                cell.recovery_reread,
                cell.recovery_failed,
            ));
        }
    }
    if let (Some(a), Some(c)) = (
        cells.iter().find(|cell| cell.engine == "append"),
        cells.iter().find(|cell| cell.engine == "dynamic"),
    ) {
        let provider_delta = signed_delta(c.provider_tokens(), a.provider_tokens());
        let round_delta = signed_delta(c.rounds, a.rounds);
        let resident_delta = signed_delta(c.resident, a.resident);
        let peak_delta = signed_delta(c.peak_resident, a.peak_resident);
        out.push_str(&format!(
            "\ndelta\n  provider cost     {provider_delta}\n  actual rounds     {round_delta}\n  resident final    {resident_delta}\n  resident peak     {peak_delta}\n"
        ));
        out.push_str(&format!(
            "\nLikely optimization target:\n{}\n",
            likely_optimization_target(scenario, a, c).as_str()
        ));
    }
    out
}

pub fn render_why_from_pair(pair_dir: &Path) -> anyhow::Result<String> {
    let pair: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(pair_dir.join("pair.json"))?)?;
    let fixture_id = pair
        .get("fixture_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let scenario = crate::context_bench::load_pack()
        .ok()
        .and_then(|pack| {
            pack.task(&fixture_id)
                .map(|task| task.file.scenario.clone())
        })
        .unwrap_or_else(|| fixture_id.split('_').next().unwrap_or("?").to_string());
    let mut cells = Vec::new();
    if let Some(rows) = pair.get("cells").and_then(|v| v.as_array()) {
        for row in rows {
            let engine = row.get("engine").and_then(|v| v.as_str()).unwrap_or("?");
            let dir = pair_dir.join(row.get("dir").and_then(|v| v.as_str()).unwrap_or(engine));
            let summary: crate::bundle::CellSummary =
                serde_json::from_str(&fs::read_to_string(dir.join("summary.json"))?)?;
            cells.push(CellView::from_summary(
                engine,
                summary.passed,
                &summary.metrics,
            ));
        }
    }
    Ok(render_why(&fixture_id, &scenario, &cells))
}

fn hash_named(hasher: &mut Sha256, label: &str, path: &Path) {
    hasher.update(label.as_bytes());
    hasher.update([0]);
    if let Ok(bytes) = fs::read(path) {
        hasher.update(&bytes);
    }
    hasher.update([0]);
}

fn hash_tree(hasher: &mut Sha256, label: &str, root: &Path) {
    hasher.update(label.as_bytes());
    hasher.update([0]);
    let mut files = Vec::new();
    collect_rel_files(root, root, &mut files);
    files.sort();
    for rel in files {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        if let Ok(bytes) = fs::read(root.join(&rel)) {
            hasher.update(&bytes);
        }
        hasher.update([0]);
    }
}

fn collect_rel_files(root: &Path, current: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rel_files(root, &path, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
}

fn signed_delta(new: u64, old: u64) -> String {
    let abs = new as i64 - old as i64;
    if old == 0 {
        return format!("{abs:+}/n/a");
    }
    let pct = abs * 100 / old as i64;
    format!("{abs:+} ({pct:+}%)")
}

fn copy_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
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

    fn cell(engine: &str, passed: bool, input: u64, resident: u64) -> CellView {
        CellView {
            engine: engine.into(),
            passed,
            input,
            output: 0,
            rounds: 10,
            resident,
            peak_resident: resident,
            forgotten: 0,
            recovered: 0,
            recovery_search: 0,
            recovery_reactivate: 0,
            recovery_reread: 0,
            recovery_failed: 0,
            compaction_in: 0,
            compaction_out: 0,
            search_calls: 0,
            search_empty: 0,
            failed_tools: 0,
        }
    }

    #[test]
    fn pack_loads_twelve_tasks_and_three_rolling_arms() {
        let pack = load_pack().expect("context-bench pack");
        assert_eq!(pack.tasks.len(), WAVE1_TASKS);
        assert_eq!(wave1_cell_count(&pack), WAVE1_CELLS);
        for &id in rolling_task_ids() {
            assert!(pack.task(id).unwrap().include_rolling(), "{id}");
        }
        for task in &pack.tasks {
            if !rolling_task_ids().contains(&task.id()) {
                assert!(!task.include_rolling(), "{}", task.id());
            }
        }
        let rendered = render_pack(&pack);
        assert!(rendered.contains("horizon_short"));
        assert!(rendered.contains("task_switch_long_b"));
        assert!(rendered.contains("schema=agent-eval.context-bench.v1"));
        assert!(rendered.contains("spec_sha256="));
        assert!(rendered.contains("pack_digest="));
    }

    #[test]
    fn spec_and_pack_digest_are_frozen() {
        assert_eq!(spec_sha256(), FROZEN_SPEC_SHA256);
        let pack = load_pack().expect("context-bench pack");
        assert_eq!(pack_digest(&pack), FROZEN_PACK_DIGEST);
        assert_eq!(spec_sha256().len(), 64);
        assert_eq!(pack_digest(&pack).len(), 64);
    }

    #[test]
    fn seed_fails_and_golden_passes_file_asserts() {
        let pack = load_pack().expect("context-bench pack");
        let report = check_pack(&pack).expect("pack self-check");
        assert!(report.contains("ok horizon_short"));
        assert_eq!(report.lines().count(), WAVE1_TASKS);
    }

    #[test]
    fn verbal_constraints_are_not_planted_in_seeds() {
        let pack = load_pack().expect("context-bench pack");
        let forbidden = [
            "unversioned ping",
            "old clients send",
            "fallback to an anonymous",
            "must not propagate",
            "must not be written into any file",
        ];
        for task in &pack.tasks {
            let seed = pack.root.join("seeds").join(&task.file.seed);
            let blob = read_tree(&seed);
            for needle in forbidden {
                assert!(
                    !blob
                        .to_ascii_lowercase()
                        .contains(&needle.to_ascii_lowercase()),
                    "{} seed {} contains verbal constraint {:?}",
                    task.id(),
                    task.file.seed,
                    needle
                );
            }
        }
        let checks = pack.root.join("checks");
        assert!(checks.join("wire_v1.py").is_file());
        assert!(checks.join("fallback_anonymous.py").is_file());
        assert!(checks.join("env_wins.py").is_file());
        assert!(checks.join("index_fix.py").is_file());
        assert!(checks.join("switch_resume.py").is_file());
        assert!(checks.join("token_now.py").is_file());
        let restated = [
            "decode must still accept",
            "unversioned ping still decodes",
            "confirm operator is allowed",
            "rate_limit is 30",
            "operator + rate_limit must still",
            "anonymous fallback, no lookup",
            "cached_load must use the same fallback",
        ];
        for id in [
            "semantic_recall",
            "semantic_recall_fallback",
            "task_switch",
            "task_switch_long_b",
        ] {
            let task = pack.task(id).unwrap();
            let last = task
                .file
                .ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    TurnOp::User { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap();
            for needle in restated {
                assert!(
                    !last
                        .to_ascii_lowercase()
                        .contains(&needle.to_ascii_lowercase()),
                    "{id} last user turn restates {needle:?}: {last}"
                );
            }
        }
        for task in &pack.tasks {
            let seed = pack.root.join("seeds").join(&task.file.seed);
            let blob = read_tree(&seed);
            assert!(
                !blob.contains("wire_v1.py") && !blob.contains("fallback_anonymous.py"),
                "{} seed must not contain hidden command sources",
                task.id()
            );
        }
    }

    #[test]
    fn horizon_variants_share_the_authbox_seed() {
        let pack = load_pack().expect("context-bench pack");
        let short = pack.task("horizon_short").unwrap();
        let long = pack.task("horizon_long").unwrap();
        assert_eq!(short.file.seed, "authbox");
        assert_eq!(long.file.seed, short.file.seed);
        assert!(short.file.ops.len() < long.file.ops.len());
    }

    fn read_tree(root: &Path) -> String {
        let mut out = String::new();
        fn walk(path: &Path, out: &mut String) {
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let child = entry.path();
                if child.is_dir() {
                    walk(&child, out);
                } else if let Ok(text) = fs::read_to_string(&child) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
        walk(root, &mut out);
        out
    }

    #[test]
    fn target_search_when_c_forgets_and_search_misses() {
        let a = cell("append", true, 1000, 1000);
        let mut c = cell("dynamic", false, 800, 200);
        c.forgotten = 5;
        c.search_calls = 3;
        c.search_empty = 3;
        assert_eq!(
            likely_optimization_target("horizon", &a, &c),
            OptimizationTarget::Search
        );
        let mut c_ok = cell("dynamic", true, 400, 200);
        c_ok.recovery_reactivate = 4;
        c_ok.recovery_search = 1;
        assert_eq!(
            likely_optimization_target("horizon", &a, &c_ok),
            OptimizationTarget::Gc
        );
    }

    #[test]
    fn target_task_anchor_on_switch_reread_spike() {
        let a = cell("append", true, 1000, 1000);
        let mut c = cell("dynamic", true, 600, 200);
        c.recovery_reread = 12;
        assert_eq!(
            likely_optimization_target("task_switch", &a, &c),
            OptimizationTarget::TaskAnchor
        );
    }

    #[test]
    fn target_compaction_when_tokens_do_not_drop() {
        let a = cell("append", true, 1000, 1000);
        let mut c = cell("dynamic", true, 950, 900);
        c.compaction_out = 40;
        assert_eq!(
            likely_optimization_target("horizon", &a, &c),
            OptimizationTarget::EpisodeCompaction
        );
    }

    #[test]
    fn target_tool_failure_when_noise_keeps_resident_and_drops_errors() {
        let mut a = cell("append", true, 1000, 2000);
        a.failed_tools = 4;
        let mut c = cell("dynamic", true, 800, 1800);
        c.failed_tools = 1;
        assert_eq!(
            likely_optimization_target("noise", &a, &c),
            OptimizationTarget::ToolFailure
        );
    }

    #[test]
    fn cell_view_maps_recovery_fields_from_summary() {
        let metrics = serde_json::json!({
            "recovery_explicit_search": 2,
            "recovery_auto_reactivation": 3,
            "recovery_workspace_reread": 4,
            "recovery_failed": 1,
            "model_input_tokens": 2,
            "model_output_tokens": 40,
            "compaction_input_tokens": 10,
            "compaction_output_tokens": 5,
            "peak_resident_bytes": 900,
        });
        let view = CellView::from_summary("dynamic", true, &metrics);
        assert_eq!(view.recovery_search, 2);
        assert_eq!(view.recovery_reactivate, 3);
        assert_eq!(view.recovery_reread, 4);
        assert_eq!(view.recovery_failed, 1);
        assert_eq!(view.output, 40);
        assert_eq!(view.peak_resident, 900);
        assert_eq!(view.provider_tokens(), 2 + 40 + 10 + 5);
        assert_eq!(signed_delta(80, 100), "-20 (-20%)");
        assert_eq!(signed_delta(120, 100), "+20 (+20%)");
    }

    #[test]
    fn golden_hidden_commands_reverify_from_report() {
        let pack = load_pack().expect("context-bench pack");
        let task = pack.task("semantic_recall").unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_task(&pack, task, dir.path()).unwrap();
        apply_golden(&pack, task, dir.path()).unwrap();
        let report = evaluate_task(&pack, task, dir.path());
        assert!(report.passed, "{report:?}");
        assert_eq!(report.kind, "file_content+command");
        assert!(crate::workload::reverify_from_report(&report).unwrap());
        let seed_dir = tempfile::tempdir().unwrap();
        seed_task(&pack, task, seed_dir.path()).unwrap();
        let seeded = evaluate_task(&pack, task, seed_dir.path());
        assert!(!seeded.passed);
        assert!(!crate::workload::reverify_from_report(&seeded).unwrap());
    }
}

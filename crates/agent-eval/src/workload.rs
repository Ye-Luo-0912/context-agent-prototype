//! A/B/C/D evaluation fixtures (TOOLS-04 remainder, M15 input).
//!
//! The four tool-surface arms the evaluation plan compares, plus the coding
//! workload fixtures each arm must solve. The fixtures are the *data*
//! half of M15: deterministic seed workspaces, model-visible task
//! descriptions (plus optional extra live turns), and hidden verification
//! hidden verification (pure file-content assertions, so they run identically
//! on every platform without an interpreter and without executing student
//! code). Executable hidden build/tests live on the suite pack, not here:
//! binding smoke fixtures to `python`/`cargo` would make CI fail-closed on
//! a missing interpreter and would exec model-written files on the host.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One tool-surface arm of the A/B/C/D comparison.
#[derive(Debug, Clone)]
pub struct ToolArm {
    /// "A" | "B" | "C" | "D".
    pub id: &'static str,
    pub name: &'static str,
    /// The tools this arm may surface to the model.
    pub tools: &'static [&'static str],
    pub note: &'static str,
}

/// The four arms from the evaluation plan. `C`/`D` reference the target ACI
/// (structured patch/process/task completion) which is not yet implemented;
/// the arm definitions stay the commitment for when those tools land.
pub const ARMS: [ToolArm; 4] = [
    ToolArm {
        id: "A",
        name: "shell-only",
        tools: &["shell.exec"],
        note: "baseline: the model gets one raw shell and no structured tools",
    },
    ToolArm {
        id: "B",
        name: "current builtin ACI",
        tools: &[
            "fs.list",
            "fs.read",
            "fs.write",
            "edit.replace",
            "search.grep",
            "git.status",
            "git.diff",
            "shell.exec",
            "context.manage",
            "capability.manage",
        ],
        note: "the shipped builtin catalog (TOOL_INVENTORY.json surface)",
    },
    ToolArm {
        id: "C",
        name: "minimal structured ACI",
        tools: &[
            "fs.list",
            "fs.read",
            "search.grep",
            "patch.apply",
            "process.run",
            "task.manage",
            "task.complete",
            "context.manage",
            "capability.manage",
        ],
        note: "target: structured patch/process/task completion replacing raw shell+write",
    },
    ToolArm {
        id: "D",
        name: "C + on-demand capability loading",
        tools: &[
            "fs.list",
            "fs.read",
            "search.grep",
            "patch.apply",
            "process.run",
            "task.manage",
            "task.complete",
            "context.manage",
            "capability.manage",
        ],
        note: "C plus capability.load-driven optional tools when a task demonstrates a need",
    },
];

/// 证据包里 hidden check 的 schema。旧 `verify.json` 只有 passed/expected_edit，
/// 加载时缺字段当不可重放，不改 summary 的 ITT 对错。
pub const VERIFY_SCHEMA: &str = "agent-eval.verify.v1";
/// 写入证据包的文件体上限。当前 smoke 文件远小于此；超限仍记 sha256，
/// 重放标 incomplete，不得假装完整复跑。
const HIDDEN_BODY_CAP: usize = 16 * 1024;

/// 一条跨平台、无解释器的 hidden 断言。题意仍是文件内容性质，不是 pytest。
#[derive(Debug, Clone, Copy)]
pub struct HiddenAssert {
    pub path: &'static str,
    pub pred: HiddenPred,
}

/// 文件内容谓词。缺文件按空串计，与原先 `unwrap_or_default` 一致。
#[derive(Debug, Clone, Copy)]
pub enum HiddenPred {
    Contains(&'static str),
    NotContains(&'static str),
    ContainsAny(&'static [&'static str]),
    MinMatches { needle: &'static str, min: usize },
}

/// One coding workload fixture: how the workspace starts, what the model is
/// asked to do, and how success is verified without a model.
#[derive(Debug, Clone)]
pub struct CodingFixture {
    pub id: &'static str,
    pub name: &'static str,
    /// The model-visible task description (what the agent is asked to do).
    pub description: &'static str,
    /// `(workspace-relative path, content)` seed files written before the
    /// task starts.
    pub seed: &'static [(&'static str, &'static str)],
    /// Hidden verification: named file-content asserts. All must pass.
    pub hidden: &'static [HiddenAssert],
    /// The expected edit, used only by the self-check tests (never exposed
    /// to the model).
    pub expected_edit: &'static str,
    /// 追加的 live 用户轮。空则 live 只发 `description`（原四题）。
    /// 非空时脚本化路径一轮一工具，对应工作集/回忆题。
    pub extra_live_turns: &'static [&'static str],
}

/// `recall_after_fix` 第一轮：修 off-by-one。后面几轮是无关噪声，最后一轮才用修好的函数。
const RECALL_TURN_FIX: &str = "The function in src/util.py reads one past the end of the list and crashes. Fix it so every element is visited and no IndexError is raised. Do not create other files yet.";
const RECALL_TURN_NOTE_1: &str = "Create src/scratch.md. Write a short note that the office coffee machine is a Breville, and the staff kitchen code is 200.";
const RECALL_TURN_NOTE_2: &str =
    "Append to src/scratch.md: the spare HDMI cable is in drawer 3, and standups are at 09:30.";
const RECALL_TURN_NOTE_3: &str = "Append to src/scratch.md: the wifi guest password is listed on the fridge, and the printer is in room 4B.";
const RECALL_TURN_REUSE: &str = "Create src/main.py that imports visit_all from util (the module is src/util.py) and prints visit_all([1, 2, 3]). Use the already-fixed visit_all; do not reintroduce the off-by-one (`i + 1`). Do not change src/util.py.";

/// Deterministic, cross-platform coding fixtures. Each one is a small real
/// task whose acceptance is a file-content property, so the same fixture
/// runs on the CI runner and on a laptop.
pub const FIXTURES: [CodingFixture; 5] = [
    CodingFixture {
        id: "fix_off_by_one",
        name: "fix an off-by-one index error",
        description: "The function in src/util.py reads one past the end of the list and crashes. Fix it so every element is visited and no IndexError is raised.",
        seed: &[(
            "src/util.py",
            "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i + 1])\n    return out\n",
        )],
        hidden: &[
            HiddenAssert {
                path: "src/util.py",
                pred: HiddenPred::NotContains("i + 1"),
            },
            HiddenAssert {
                path: "src/util.py",
                pred: HiddenPred::Contains("range(len(items))"),
            },
        ],
        expected_edit: "replace `items[i + 1]` with `items[i]`",
        extra_live_turns: &[],
    },
    CodingFixture {
        id: "implement_stub",
        name: "implement the stubbed function",
        description: "src/math.py declares `double(x)` but leaves it as a stub that returns None. Implement it so it returns twice its argument.",
        seed: &[(
            "src/math.py",
            "def double(x):\n    # TODO: implement\n    pass\n",
        )],
        hidden: &[
            HiddenAssert {
                path: "src/math.py",
                pred: HiddenPred::NotContains("pass"),
            },
            HiddenAssert {
                path: "src/math.py",
                pred: HiddenPred::ContainsAny(&["return x * 2", "return 2 * x"]),
            },
        ],
        expected_edit: "replace the `pass` stub with `return x * 2`",
        extra_live_turns: &[],
    },
    CodingFixture {
        id: "rename_symbol",
        name: "rename a symbol everywhere it is used",
        description: "src/app.py uses the variable `old_name` in three places. Rename it to `new_name` in every place; no reference may keep the old name.",
        seed: &[(
            "src/app.py",
            "old_name = \"value\"\nprint(old_name)\ndef use():\n    return old_name\n",
        )],
        hidden: &[
            HiddenAssert {
                path: "src/app.py",
                pred: HiddenPred::NotContains("old_name"),
            },
            HiddenAssert {
                path: "src/app.py",
                pred: HiddenPred::MinMatches {
                    needle: "new_name",
                    min: 3,
                },
            },
        ],
        expected_edit: "rename all three `old_name` occurrences to `new_name`",
        extra_live_turns: &[],
    },
    CodingFixture {
        id: "add_test",
        name: "add a test for an existing function",
        description: "src/calc.py defines `add(a, b)`. Append a `def test_add()` function to that same file (`src/calc.py`) that asserts `add` behaves correctly for a non-trivial case. Do not create a new test file.",
        seed: &[("src/calc.py", "def add(a, b):\n    return a + b\n")],
        hidden: &[
            HiddenAssert {
                path: "src/calc.py",
                pred: HiddenPred::ContainsAny(&["def test_add", "assert add("]),
            },
            HiddenAssert {
                path: "src/calc.py",
                pred: HiddenPred::NotContains("# TODO"),
            },
        ],
        expected_edit: "append a `def test_add():` block asserting `add` in `src/calc.py`",
        extra_live_turns: &[],
    },
    CodingFixture {
        id: "recall_after_fix",
        name: "reuse a fix after unrelated notes",
        description: RECALL_TURN_FIX,
        seed: &[(
            "src/util.py",
            "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i + 1])\n    return out\n",
        )],
        hidden: &[
            HiddenAssert {
                path: "src/util.py",
                pred: HiddenPred::NotContains("i + 1"),
            },
            HiddenAssert {
                path: "src/util.py",
                pred: HiddenPred::Contains("range(len(items))"),
            },
            HiddenAssert {
                path: "src/scratch.md",
                pred: HiddenPred::Contains("Breville"),
            },
            HiddenAssert {
                path: "src/scratch.md",
                pred: HiddenPred::Contains("200"),
            },
            HiddenAssert {
                path: "src/scratch.md",
                pred: HiddenPred::Contains("HDMI"),
            },
            HiddenAssert {
                path: "src/scratch.md",
                pred: HiddenPred::Contains("4B"),
            },
            HiddenAssert {
                path: "src/main.py",
                pred: HiddenPred::Contains("visit_all"),
            },
            HiddenAssert {
                path: "src/main.py",
                pred: HiddenPred::NotContains("i + 1"),
            },
        ],
        expected_edit: "fix util, write the scratch notes, then add main.py that calls visit_all without reintroducing i + 1",
        extra_live_turns: &[
            RECALL_TURN_NOTE_1,
            RECALL_TURN_NOTE_2,
            RECALL_TURN_NOTE_3,
            RECALL_TURN_REUSE,
        ],
    },
];

/// Live 用户轮：`description` 加 `extra_live_turns`。原四题仍是一轮。
pub fn live_turns(fixture: &CodingFixture) -> Vec<String> {
    let mut turns = vec![fixture.description.to_string()];
    turns.extend(
        fixture
            .extra_live_turns
            .iter()
            .map(|turn| (*turn).to_string()),
    );
    turns
}

/// 多轮 live 题：脚本化对照一轮只发一个 tool，再 `done` 结束该 turn。
pub fn scripted_one_tool_per_turn(fixture: &CodingFixture) -> bool {
    !fixture.extra_live_turns.is_empty()
}

/// Write the seed files of one fixture into a fresh workspace root.
pub fn seed_fixture(fixture: &CodingFixture, root: &std::path::Path) {
    for (path, content) in fixture.seed {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, content).unwrap();
    }
}

/// 对工作区跑完全部 hidden 断言，并带上有界文件体，供证据包重放。
pub fn evaluate_hidden(fixture: &CodingFixture, root: &Path) -> HiddenReport {
    let mut bodies: BTreeMap<String, HiddenFileBody> = BTreeMap::new();
    for assert in fixture.hidden {
        bodies
            .entry(assert.path.to_string())
            .or_insert_with(|| read_hidden_file(root, assert.path));
    }
    let assertions: Vec<HiddenAssertionResult> = fixture
        .hidden
        .iter()
        .map(|assert| {
            let file = bodies
                .get(assert.path)
                .expect("every hidden path is collected");
            eval_assert(assert, file)
        })
        .collect();
    HiddenReport {
        schema: VERIFY_SCHEMA.to_string(),
        kind: "file_content".into(),
        fixture_id: fixture.id.to_string(),
        expected_edit: fixture.expected_edit.to_string(),
        passed: assertions.iter().all(|row| row.passed),
        replay_complete: bodies.values().all(|file| !file.truncated),
        assertions,
        files: bodies.into_values().collect(),
        commands: Vec::new(),
    }
}

/// Whether the fixture currently passes its hidden verification.
pub fn fixture_passes(fixture: &CodingFixture, root: &Path) -> bool {
    evaluate_hidden(fixture, root).passed
}

/// 用证据包里保存的文件体重跑断言。工作区已删时仍可核对。
/// 截断或不完整的包返回 `Err`，不得把缺体当成通过。
pub fn reverify_from_report(report: &HiddenReport) -> anyhow::Result<bool> {
    if report.schema != VERIFY_SCHEMA {
        anyhow::bail!("unsupported verify schema {}", report.schema);
    }
    if report.kind == "hidden_command" {
        if !report.replay_complete || report.commands.is_empty() {
            anyhow::bail!("hidden command report is incomplete");
        }
        return Ok(report.commands.iter().all(|row| row.passed));
    }
    if report.kind != "file_content" {
        anyhow::bail!("unsupported hidden kind {}", report.kind);
    }
    if !report.replay_complete || report.files.iter().any(|file| file.truncated) {
        anyhow::bail!("hidden file bodies were truncated; replay is incomplete");
    }
    let mut by_path: BTreeMap<&str, &HiddenFileBody> = BTreeMap::new();
    for file in &report.files {
        by_path.insert(file.path.as_str(), file);
    }
    let mut passed = true;
    for row in &report.assertions {
        let file = by_path
            .get(row.path.as_str())
            .ok_or_else(|| anyhow::anyhow!("verify report missing body for {}", row.path))?;
        let needles: Vec<&str> = row.needles.iter().map(String::as_str).collect();
        let (ok, _) = eval_pred(&file.body, &row.pred, &needles, row.min);
        if !ok {
            passed = false;
        }
    }
    Ok(passed)
}

fn read_hidden_file(root: &Path, rel: &str) -> HiddenFileBody {
    let path = root.join(rel);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let digest = hex_encode(&Sha256::digest(&bytes));
            let truncated = bytes.len() > HIDDEN_BODY_CAP;
            let slice = if truncated {
                &bytes[..HIDDEN_BODY_CAP]
            } else {
                &bytes
            };
            HiddenFileBody {
                path: rel.to_string(),
                exists: true,
                sha256: digest,
                bytes: bytes.len(),
                truncated,
                body: String::from_utf8_lossy(slice).into_owned(),
            }
        }
        Err(_) => HiddenFileBody {
            path: rel.to_string(),
            exists: false,
            sha256: hex_encode(&Sha256::digest([])),
            bytes: 0,
            truncated: false,
            body: String::new(),
        },
    }
}

fn eval_assert(assert: &HiddenAssert, file: &HiddenFileBody) -> HiddenAssertionResult {
    let (kind, needles, min) = pred_parts(assert.pred);
    let (passed, count) = eval_pred(&file.body, kind, &needles, min);
    HiddenAssertionResult {
        path: assert.path.to_string(),
        pred: kind.to_string(),
        needles: needles.iter().map(|needle| (*needle).to_string()).collect(),
        min,
        count: Some(count),
        passed,
        file_exists: file.exists,
    }
}

fn pred_parts(pred: HiddenPred) -> (&'static str, Vec<&'static str>, Option<usize>) {
    match pred {
        HiddenPred::Contains(needle) => ("contains", vec![needle], None),
        HiddenPred::NotContains(needle) => ("not_contains", vec![needle], None),
        HiddenPred::ContainsAny(needles) => ("contains_any", needles.to_vec(), None),
        HiddenPred::MinMatches { needle, min } => ("min_matches", vec![needle], Some(min)),
    }
}

fn eval_pred(content: &str, kind: &str, needles: &[&str], min: Option<usize>) -> (bool, usize) {
    match kind {
        "contains" => {
            let needle = needles.first().copied().unwrap_or("");
            let count = content.matches(needle).count();
            (count > 0, count)
        }
        "not_contains" => {
            let needle = needles.first().copied().unwrap_or("");
            let count = content.matches(needle).count();
            (count == 0, count)
        }
        "contains_any" => {
            let count = needles
                .iter()
                .filter(|needle| content.contains(*needle))
                .count();
            (count > 0, count)
        }
        "min_matches" => {
            let needle = needles.first().copied().unwrap_or("");
            let count = content.matches(needle).count();
            (count >= min.unwrap_or(0), count)
        }
        _ => (false, 0),
    }
}

pub(crate) fn hash_hidden(fixture: &CodingFixture, hasher: &mut Sha256) {
    for assert in fixture.hidden {
        hasher.update(assert.path.as_bytes());
        hasher.update(b"\n");
        let (kind, needles, min) = pred_parts(assert.pred);
        hasher.update(kind.as_bytes());
        hasher.update(b"\n");
        for needle in needles {
            hasher.update(needle.as_bytes());
            hasher.update(b"\n");
        }
        if let Some(min) = min {
            hasher.update(min.to_string().as_bytes());
            hasher.update(b"\n");
        }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenReport {
    pub schema: String,
    pub kind: String,
    pub fixture_id: String,
    pub expected_edit: String,
    pub passed: bool,
    /// 所有被检查文件的体都完整写入（未截断）。
    #[serde(default)]
    pub replay_complete: bool,
    pub assertions: Vec<HiddenAssertionResult>,
    pub files: Vec<HiddenFileBody>,
    /// 可执行 hidden 命令的捕获记录。空表示本题仍是纯文件断言。
    #[serde(default)]
    pub commands: Vec<HiddenCommandResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenAssertionResult {
    pub path: String,
    pub pred: String,
    pub needles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    pub passed: bool,
    pub file_exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenFileBody {
    pub path: String,
    pub exists: bool,
    pub sha256: String,
    pub bytes: usize,
    pub truncated: bool,
    pub body: String,
}

/// 一次 hidden 命令的有界捕获。stdout/stderr 截断后仍保留退出码。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HiddenCommandResult {
    pub argv: Vec<String>,
    pub expect_exit: i32,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    pub passed: bool,
}

/// Self-check the evaluation inputs: every fixture's seed must be writable
/// and its hidden verification must *fail* in the seeded state (so the
/// acceptance genuinely tests the intended change). Used by
/// `agent-eval --fixtures` before listing, and by the unit tests.
pub fn verify_fixture_inputs() -> anyhow::Result<()> {
    for fixture in &FIXTURES {
        let dir = tempfile::tempdir()
            .map_err(|error| anyhow::anyhow!("create temp workspace: {error}"))?;
        seed_fixture(fixture, dir.path());
        if fixture_passes(fixture, dir.path()) {
            anyhow::bail!(
                "fixture '{}' passes its hidden verification in the seeded state — \
                 the acceptance does not test the intended change",
                fixture.id
            );
        }
        for (path, _) in fixture.seed {
            let target = dir.path().join(path);
            if !target.exists() {
                anyhow::bail!(
                    "fixture '{}' seed file missing after seeding: {path}",
                    fixture.id
                );
            }
        }
    }
    Ok(())
}

pub fn fixture_class(id: &str) -> &'static str {
    match id {
        "fix_off_by_one" => "bug",
        "implement_stub" => "feature",
        "rename_symbol" => "refactor",
        "add_test" => "test",
        "recall_after_fix" => "recall",
        _ => "other",
    }
}

pub fn fixture_role(id: &str) -> &'static str {
    if id == "recall_after_fix" {
        "diagnostic"
    } else {
        "smoke"
    }
}

/// Human-readable listing of the M15 evaluation inputs: the four arms and
/// the coding fixtures, used by `agent-eval --fixtures`.
pub fn render_fixtures() -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "acceptance suite: frozen (pack). smoke fixtures {}/{} are diagnostic only.\n",
        FIXTURES.len(),
        crate::analysis::MIN_TASKS,
    ));
    if let Ok(pack) = crate::suite::load_pack() {
        out.push_str(&format!(
            "suite pack: {}/{} frozen={}. smoke FIXTURES below do not count.\n\n",
            pack.tasks.len(),
            pack.manifest.target_n,
            pack.frozen()
        ));
    } else {
        out.push('\n');
    }
    if let Ok(pack) = crate::suite::load_pack() {
        out.push_str(&format!(
            "suite pack: {}/{} frozen={}. smoke FIXTURES below do not count.\n\n",
            pack.tasks.len(),
            pack.manifest.target_n,
            pack.frozen()
        ));
    } else {
        out.push('\n');
    }
    out.push_str("A/B/C/D tool-surface arms:\n");
    for arm in &ARMS {
        out.push_str(&format!(
            "  arm {} — {}: {}\n    tools: {}\n",
            arm.id,
            arm.name,
            arm.note,
            arm.tools.join(", ")
        ));
    }
    out.push_str("\ncoding workload fixtures (seed + hidden verification):\n");
    for fixture in &FIXTURES {
        out.push_str(&format!(
            "  {} — {} [{} / {}]\n",
            fixture.id,
            fixture.name,
            fixture_class(fixture.id),
            fixture_role(fixture.id)
        ));
        out.push_str(&format!("    task: {}\n", fixture.description));
        let files: Vec<&str> = fixture.seed.iter().map(|(path, _)| *path).collect();
        out.push_str(&format!("    seeds: {}\n", files.join(", ")));
        out.push_str(&format!("    expected edit: {}\n", fixture.expected_edit));
        out.push_str(&format!(
            "    hidden: {} file_content asserts\n",
            fixture.hidden.len()
        ));
        let turns = live_turns(fixture);
        if turns.len() > 1 {
            out.push_str(&format!("    live turns: {}\n", turns.len()));
            for (index, turn) in turns.iter().enumerate() {
                out.push_str(&format!("      {}: {}\n", index + 1, turn));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_fixtures_are_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for fixture in &FIXTURES {
            assert!(!fixture.id.is_empty());
            assert!(!fixture.name.is_empty());
            assert!(!fixture.description.is_empty());
            assert!(!fixture.expected_edit.is_empty());
            assert!(
                !fixture.hidden.is_empty(),
                "fixture '{}' needs at least one hidden assert",
                fixture.id
            );
            for assert in fixture.hidden {
                assert!(
                    !std::path::Path::new(assert.path).is_absolute(),
                    "hidden path must be workspace-relative: {}",
                    assert.path
                );
                let (_, needles, _) = pred_parts(assert.pred);
                assert!(
                    !needles.is_empty() && needles.iter().all(|needle| !needle.is_empty()),
                    "hidden assert on {} has an empty needle",
                    assert.path
                );
            }
            assert!(
                ids.insert(fixture.id),
                "duplicate fixture id: {}",
                fixture.id
            );
            for (path, content) in fixture.seed {
                assert!(
                    !std::path::Path::new(path).is_absolute(),
                    "seed path must be workspace-relative: {path}"
                );
                assert!(
                    !content.is_empty(),
                    "seed content must not be empty: {path}"
                );
            }
        }
        assert_eq!(FIXTURES.len(), 5);
        for fixture in &FIXTURES {
            assert_ne!(
                fixture_class(fixture.id),
                "other",
                "every current fixture needs an explicit class: {}",
                fixture.id
            );
        }
        let add_test = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "add_test")
            .unwrap();
        assert!(
            add_test.description.contains("src/calc.py")
                && add_test
                    .description
                    .contains("Do not create a new test file"),
            "add_test 题面必须钉到已有 hidden check 的文件，不能把校验改宽"
        );
        let recall = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "recall_after_fix")
            .unwrap();
        assert_eq!(live_turns(recall).len(), 5);
        assert!(scripted_one_tool_per_turn(recall));
        assert!(!scripted_one_tool_per_turn(add_test));
    }

    #[test]
    fn every_fixture_fails_verification_before_the_fix() {
        for fixture in &FIXTURES {
            let dir = tempfile::tempdir().unwrap();
            seed_fixture(fixture, dir.path());
            assert!(
                !fixture_passes(fixture, dir.path()),
                "fixture '{}' must fail verification in its seeded state",
                fixture.id
            );
        }
    }

    #[test]
    fn every_fixture_passes_verification_after_the_expected_edit() {
        // Apply each fixture's expected edit by hand, then verify. This
        // proves the hidden check accepts exactly the intended outcome and
        // rejects the seeded state (the test above).
        let cases: [(&str, &[(&str, &str)]); 5] = [
            (
                "fix_off_by_one",
                &[(
                    "src/util.py",
                    "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i])\n    return out\n",
                )],
            ),
            (
                "implement_stub",
                &[("src/math.py", "def double(x):\n    return x * 2\n")],
            ),
            (
                "rename_symbol",
                &[(
                    "src/app.py",
                    "new_name = \"value\"\nprint(new_name)\ndef use():\n    return new_name\n",
                )],
            ),
            (
                "add_test",
                &[(
                    "src/calc.py",
                    "def add(a, b):\n    return a + b\n\ndef test_add():\n    assert add(2, 3) == 5\n",
                )],
            ),
            (
                "recall_after_fix",
                &[
                    (
                        "src/util.py",
                        "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i])\n    return out\n",
                    ),
                    (
                        "src/scratch.md",
                        "The office coffee machine is a Breville. The staff kitchen code is 200.\nThe spare HDMI cable is in drawer 3. Standups are at 09:30.\nThe wifi guest password is listed on the fridge. The printer is in room 4B.\n",
                    ),
                    (
                        "src/main.py",
                        "from util import visit_all\nprint(visit_all([1, 2, 3]))\n",
                    ),
                ],
            ),
        ];
        for (id, files) in cases {
            let fixture = FIXTURES
                .iter()
                .find(|fixture| fixture.id == id)
                .expect("fixture exists");
            let dir = tempfile::tempdir().unwrap();
            for (path, content) in files {
                let target = dir.path().join(path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(target, content).unwrap();
            }
            assert!(
                fixture_passes(fixture, dir.path()),
                "fixture '{}' must pass after the expected edit",
                fixture.id
            );
        }
    }

    #[test]
    fn recall_after_fix_fails_if_main_reintroduces_the_bug() {
        let fixture = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "recall_after_fix")
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/util.py"),
            "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i])\n    return out\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/scratch.md"), "Breville 200 HDMI 4B\n").unwrap();
        std::fs::write(
            dir.path().join("src/main.py"),
            "def visit_all(items):\n    return [items[i + 1] for i in range(len(items))]\n",
        )
        .unwrap();
        assert!(
            !fixture_passes(fixture, dir.path()),
            "reintroducing i + 1 in main.py must fail the hidden check"
        );
        let report = evaluate_hidden(fixture, dir.path());
        assert!(!report.passed);
        assert!(
            report.assertions.iter().any(|row| {
                row.path == "src/main.py" && row.pred == "not_contains" && !row.passed
            }),
            "the failing assert must name src/main.py not_contains i + 1: {:?}",
            report.assertions
        );
        assert!(reverify_from_report(&report).unwrap() == report.passed);
    }

    #[test]
    fn hidden_report_replays_after_workspace_is_gone() {
        let fixture = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "add_test")
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/calc.py"),
            "def add(a, b):\n    return a + b\n\ndef test_add():\n    assert add(2, 3) == 5\n",
        )
        .unwrap();
        let report = evaluate_hidden(fixture, dir.path());
        assert!(report.passed);
        assert!(report.replay_complete);
        drop(dir);
        assert!(reverify_from_report(&report).unwrap());
    }

    #[test]
    fn recall_missing_main_names_the_failed_assert() {
        let fixture = FIXTURES
            .iter()
            .find(|fixture| fixture.id == "recall_after_fix")
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/util.py"),
            "def visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i])\n    return out\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/scratch.md"), "Breville 200 HDMI 4B\n").unwrap();
        let report = evaluate_hidden(fixture, dir.path());
        assert!(!report.passed);
        let main = report
            .assertions
            .iter()
            .find(|row| row.path == "src/main.py" && row.pred == "contains")
            .expect("visit_all assert");
        assert!(!main.file_exists);
        assert!(!main.passed);
    }

    #[test]
    fn the_four_arms_are_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for arm in &ARMS {
            assert!(ids.insert(arm.id), "duplicate arm id: {}", arm.id);
            assert!(!arm.tools.is_empty(), "arm {} has no tools", arm.id);
        }
        assert_eq!(ARMS.len(), 4);
        // Arm A is the shell-only baseline; every other arm includes the
        // read/discovery core.
        assert_eq!(ARMS[0].id, "A");
        assert_eq!(ARMS[0].tools, &["shell.exec"][..]);
        for arm in &ARMS[1..] {
            assert!(
                arm.tools.contains(&"fs.read"),
                "arm {} lacks fs.read",
                arm.id
            );
        }
    }
}

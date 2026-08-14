//! A/B/C/D evaluation fixtures (TOOLS-04 remainder, M15 input).
//!
//! The four tool-surface arms the evaluation plan compares, plus the coding
//! workload fixtures each arm must solve. The fixtures are the *data*
//! half of M15: deterministic seed workspaces, model-visible task
//! descriptions (plus optional extra live turns), and hidden verification
//! (pure file-content assertions, so they run identically on every
//! platform without an interpreter). The live A/B/C/D run against a real
//! model is the M15 acceptance; this module makes the inputs well-formed
//! and self-checked first.

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
    /// Hidden verification: must return `true` only after the task is done
    /// correctly. Reads the workspace root.
    pub verify: fn(&std::path::Path) -> bool,
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
        verify: |root| {
            let content = std::fs::read_to_string(root.join("src/util.py")).unwrap_or_default();
            !content.contains("i + 1") && content.contains("range(len(items))")
        },
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
        verify: |root| {
            let content = std::fs::read_to_string(root.join("src/math.py")).unwrap_or_default();
            !content.contains("pass")
                && (content.contains("return x * 2") || content.contains("return 2 * x"))
        },
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
        verify: |root| {
            let content = std::fs::read_to_string(root.join("src/app.py")).unwrap_or_default();
            !content.contains("old_name") && content.matches("new_name").count() >= 3
        },
        expected_edit: "rename all three `old_name` occurrences to `new_name`",
        extra_live_turns: &[],
    },
    CodingFixture {
        id: "add_test",
        name: "add a test for an existing function",
        description: "src/calc.py defines `add(a, b)`. Append a `def test_add()` function to that same file (`src/calc.py`) that asserts `add` behaves correctly for a non-trivial case. Do not create a new test file.",
        seed: &[("src/calc.py", "def add(a, b):\n    return a + b\n")],
        verify: |root| {
            let content = std::fs::read_to_string(root.join("src/calc.py")).unwrap_or_default();
            (content.contains("def test_add") || content.contains("assert add("))
                && !content.contains("# TODO")
        },
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
        verify: |root| {
            let util = std::fs::read_to_string(root.join("src/util.py")).unwrap_or_default();
            let scratch = std::fs::read_to_string(root.join("src/scratch.md")).unwrap_or_default();
            let main = std::fs::read_to_string(root.join("src/main.py")).unwrap_or_default();
            !util.contains("i + 1")
                && util.contains("range(len(items))")
                && scratch.contains("Breville")
                && scratch.contains("200")
                && scratch.contains("HDMI")
                && scratch.contains("4B")
                && main.contains("visit_all")
                && !main.contains("i + 1")
        },
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

/// Whether the fixture currently passes its hidden verification.
pub fn fixture_passes(fixture: &CodingFixture, root: &std::path::Path) -> bool {
    (fixture.verify)(root)
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
        "acceptance suite: not frozen ({}/{}). current fixtures are smoke/diagnostic only.\n\n",
        FIXTURES.len(),
        crate::analysis::MIN_TASKS,
    ));
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

//! EVAL-01.4：接受套件包。300 题是审查过的交付物，不是 `FIXTURES` 烟雾。
//!
//! 冻结条件由本模块计算：`manifest.frozen` 不能单独打开门禁。9 道文件题
//! 加上 500 道 SWE-bench Verified 使 n≥300。异质性、出处、可执行 hidden
//! 已审查；pack 已冻结。EVAL-01.3c 将 `SUITE_FROZEN`、exact 300 acceptance
//! ids 与 SPEC 一并冻结。门禁是这 300 题，不是 509 包里任意子集。
//! 300×3 接受细胞仍须先做剩余校准。不得发明一行 stand-in。
//!
//! `evaluate_suite_task` 由测试、`--suite-check` 和 `--pilot-run` 调用。

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::workload::HiddenCommandResult;

pub const SUITE_SCHEMA: &str = "agent-eval.suite.v1";
pub const TARGET_N: usize = 300;
const STDOUT_CAP: usize = 32 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MIN_DESCRIPTION: usize = 80;
const MIN_SEED_BYTES: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteManifest {
    pub schema: String,
    pub frozen: bool,
    pub target_n: usize,
    pub review: SuiteReview,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteReview {
    pub heterogeneity_reviewed: bool,
    pub provenance_reviewed: bool,
    pub executable_hidden_reviewed: bool,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSource {
    pub kind: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSeedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteHiddenFile {
    pub path: String,
    pub pred: String,
    pub needles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteHiddenCommand {
    pub argv: Vec<String>,
    #[serde(default)]
    pub timeout_ms: u64,
    #[serde(default)]
    pub expect_exit: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteTask {
    pub id: String,
    pub name: String,
    /// bug | feature | refactor | test | recall
    pub class: String,
    pub language: String,
    /// small | medium | large
    pub size: String,
    pub edit_shape: String,
    /// none | notes-then-reuse | long-gap
    pub recall_pressure: String,
    pub source: SuiteSource,
    pub description: String,
    #[serde(default)]
    pub extra_live_turns: Vec<String>,
    /// 模型可见的工作区。不得包含 hidden overlay。
    #[serde(default)]
    pub seed: Vec<SuiteSeedFile>,
    /// 仅在 hidden 验证时写入，模型看不到。
    #[serde(default)]
    pub hidden_overlay: Vec<SuiteSeedFile>,
    /// 自检用的已知正确补丁，永不展示给模型。
    #[serde(default)]
    pub expected_files: Vec<SuiteSeedFile>,
    #[serde(default)]
    pub hidden_files: Vec<SuiteHiddenFile>,
    #[serde(default)]
    pub hidden_commands: Vec<SuiteHiddenCommand>,
    /// `files`（默认）或 `swebench-docker`。后者用官方镜像评测，不在单元测试里拉镜像。
    #[serde(default)]
    pub runtime: String,
}

#[derive(Debug, Clone)]
pub struct SuitePack {
    pub manifest: SuiteManifest,
    pub tasks: Vec<SuiteTask>,
    pub blockers: Vec<String>,
}

impl SuitePack {
    pub fn frozen(&self) -> bool {
        self.manifest.frozen && self.blockers.is_empty()
    }
}

pub fn suite_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("suite")
}

pub fn load_pack() -> anyhow::Result<SuitePack> {
    load_pack_from(&suite_dir())
}

pub fn load_pack_from(root: &Path) -> anyhow::Result<SuitePack> {
    let manifest_path = root.join("manifest.json");
    let manifest: SuiteManifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    if manifest.schema != SUITE_SCHEMA {
        anyhow::bail!("unsupported suite schema {}", manifest.schema);
    }
    if manifest.target_n != TARGET_N {
        anyhow::bail!(
            "suite target_n={} must equal TARGET_N={TARGET_N}",
            manifest.target_n
        );
    }
    let mut tasks = Vec::new();
    let tasks_dir = root.join("tasks");
    if tasks_dir.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(&tasks_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                let task_json = path.join("task.json");
                if task_json.is_file() {
                    tasks.push(load_dir_task(&path)?);
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            tasks.push(load_json_task(&path)?);
        }
    }
    let swebench = root.join("swebench-verified.jsonl");
    if swebench.is_file() {
        tasks.extend(crate::harvest::load_swebench_jsonl(&swebench)?);
    }
    let blockers = freeze_blockers(&manifest, &tasks);
    if manifest.frozen && !blockers.is_empty() {
        anyhow::bail!(
            "manifest.frozen=true but freeze blockers remain: {}",
            blockers.join("; ")
        );
    }
    Ok(SuitePack {
        manifest,
        tasks,
        blockers,
    })
}

fn load_json_task(path: &Path) -> anyhow::Result<SuiteTask> {
    let task: SuiteTask = serde_json::from_str(&fs::read_to_string(path)?)?;
    validate_task_files(&task)?;
    Ok(task)
}

/// `tasks/<id>/task.json` + `seed/` / `hidden/` / `expected/` 目录。
/// 目录在场时覆盖 JSON 里同名字段，这样收割题不必把源码塞进 JSON。
fn load_dir_task(dir: &Path) -> anyhow::Result<SuiteTask> {
    let mut task: SuiteTask = serde_json::from_str(&fs::read_to_string(dir.join("task.json"))?)?;
    let seed_dir = dir.join("seed");
    if seed_dir.is_dir() {
        task.seed = collect_rel_files(&seed_dir)?;
    }
    let hidden_dir = dir.join("hidden");
    if hidden_dir.is_dir() {
        task.hidden_overlay = collect_rel_files(&hidden_dir)?;
    }
    let expected_dir = dir.join("expected");
    if expected_dir.is_dir() {
        task.expected_files = collect_rel_files(&expected_dir)?;
    }
    validate_task_files(&task)?;
    Ok(task)
}

fn validate_task_files(task: &SuiteTask) -> anyhow::Result<()> {
    for file in task
        .seed
        .iter()
        .chain(task.hidden_overlay.iter())
        .chain(task.expected_files.iter())
    {
        reject_escape(&file.path)?;
    }
    Ok(())
}

fn collect_rel_files(root: &Path) -> anyhow::Result<Vec<SuiteSeedFile>> {
    let mut files = Vec::new();
    collect_walk(root, root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn collect_walk(root: &Path, dir: &Path, files: &mut Vec<SuiteSeedFile>) -> anyhow::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect_walk(root, &path, files)?;
            continue;
        }
        let rel = path.strip_prefix(root)?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-utf8 suite path {}", path.display()))?
            .replace('\\', "/");
        reject_escape(&rel_str)?;
        files.push(SuiteSeedFile {
            path: rel_str,
            content: fs::read_to_string(&path)?,
        });
    }
    Ok(())
}

fn reject_escape(rel: &str) -> anyhow::Result<()> {
    if rel.is_empty() || rel.contains('\0') {
        anyhow::bail!("illegal suite path {rel:?}");
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        anyhow::bail!("absolute suite path {rel}");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => anyhow::bail!("illegal suite path {rel}"),
        }
    }
    Ok(())
}

/// 把模型可见 seed 写到工作区。
pub fn materialize_seed(task: &SuiteTask, root: &Path) -> anyhow::Result<()> {
    apply_files(root, &task.seed)
}

/// 套件 live 的用户轮：题面 + `extra_live_turns`（recall 题才有 distractor）。
pub fn live_turns(task: &SuiteTask) -> Vec<String> {
    let mut turns = vec![task.description.clone()];
    turns.extend(task.extra_live_turns.iter().cloned());
    turns
}

/// 文件题写 seed；SWE-bench 从冻结的 GitHub `base_commit` 克隆到工作区。
pub fn materialize_live_workspace(task: &SuiteTask, root: &Path) -> anyhow::Result<()> {
    if task.runtime == crate::harvest::RUNTIME {
        let instance = crate::harvest::instance_id_from_suite_id(&task.id)
            .ok_or_else(|| anyhow::anyhow!("{} is not a swebench suite id", task.id))?;
        let repo = task
            .source
            .repo
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{} missing repo URL", task.id))?;
        let commit = task
            .source
            .commit
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("{} missing base_commit", task.id))?;
        let cache = crate::harvest::ensure_checkout(repo, commit, instance)?;
        crate::harvest::clone_engine_workspace(&cache, root)
    } else {
        materialize_seed(task, root)?;
        ensure_workspace_git(root)
    }
}

/// File-only eval workspaces are not clones. Init a local git repo so
/// `git.status` / `git.diff` are real probes, not "not a git repository".
/// Do not hide those tools. SWE-bench clones already have `.git` and skip.
pub fn ensure_workspace_git(root: &Path) -> anyhow::Result<()> {
    if root.join(".git").exists() {
        return Ok(());
    }
    fs::create_dir_all(root)?;
    git_ok(root, &["init", "--quiet"])?;
    // Keep this repo's line endings off WinINET/core.autocrlf so
    // `git.status` is not a wall of CRLF noise on Windows live cells.
    git_ok(root, &["config", "core.autocrlf", "false"])?;
    let gitignore = root.join(".gitignore");
    if !gitignore.exists() {
        fs::write(&gitignore, ".focus-agent/\n")?;
    }
    git_ok(root, &["add", "-A"])?;
    let status = Command::new("git")
        .args([
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "eval seed",
            "--quiet",
            "--allow-empty",
        ])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "agent-eval")
        .env("GIT_AUTHOR_EMAIL", "eval@invalid")
        .env("GIT_COMMITTER_NAME", "agent-eval")
        .env("GIT_COMMITTER_EMAIL", "eval@invalid")
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("git commit eval seed failed with {status}");
    }
    Ok(())
}

fn git_ok(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("git {args:?} failed with {status}");
    }
    Ok(())
}

pub fn apply_files(root: &Path, files: &[SuiteSeedFile]) -> anyhow::Result<()> {
    for file in files {
        reject_escape(&file.path)?;
        let dest = root.join(&file.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest, &file.content)?;
    }
    Ok(())
}

/// 复制工作区、叠 hidden overlay、跑 hidden commands。不把 expected 补丁交给模型。
pub fn evaluate_suite_task(
    task: &SuiteTask,
    workspace: &Path,
) -> anyhow::Result<Vec<HiddenCommandResult>> {
    if task.runtime == crate::harvest::RUNTIME {
        return Ok(task
            .hidden_commands
            .iter()
            .map(|spec| HiddenCommandResult {
                argv: spec.argv.clone(),
                expect_exit: spec.expect_exit,
                stderr: "swebench-docker eval is opt-in; unit tests do not pull images".into(),
                passed: false,
                ..HiddenCommandResult::default()
            })
            .collect());
    }
    evaluate_overlay_commands(workspace, &task.hidden_overlay, &task.hidden_commands)
}

/// 复制工作区、叠 overlay、跑命令。overlay 不写回模型工作区。
pub fn evaluate_overlay_commands(
    workspace: &Path,
    overlay: &[SuiteSeedFile],
    commands: &[SuiteHiddenCommand],
) -> anyhow::Result<Vec<HiddenCommandResult>> {
    let tmp = tempfile::tempdir()?;
    copy_tree(workspace, tmp.path())?;
    apply_files(tmp.path(), overlay)?;
    Ok(commands
        .iter()
        .map(|spec| run_hidden_command(tmp.path(), spec))
        .collect())
}

pub fn all_hidden_passed(results: &[HiddenCommandResult]) -> bool {
    !results.is_empty() && results.iter().all(|row| row.passed)
}

/// 文件题的 seed 必须失败、expected 必须通过。Docker 题不在这里拉镜像。
pub fn check_file_harvest(pack: &SuitePack) -> anyhow::Result<String> {
    let mut n = 0u32;
    for task in &pack.tasks {
        if task.runtime == crate::harvest::RUNTIME {
            continue;
        }
        n += 1;
        let seed_root = tempfile::tempdir()?;
        materialize_seed(task, seed_root.path())?;
        let failed = evaluate_suite_task(task, seed_root.path())?;
        if all_hidden_passed(&failed) {
            anyhow::bail!("{} seed must fail hidden command", task.id);
        }
        let fixed_root = tempfile::tempdir()?;
        materialize_seed(task, fixed_root.path())?;
        apply_files(fixed_root.path(), &task.expected_files)?;
        let passed = evaluate_suite_task(task, fixed_root.path())?;
        if !all_hidden_passed(&passed) {
            anyhow::bail!("{} expected fix must pass hidden command", task.id);
        }
    }
    if n == 0 {
        anyhow::bail!("no file-harvested tasks to self-check");
    }
    Ok(format!("file-harvested self-check ok n={n}"))
}

fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

pub fn freeze_blockers(manifest: &SuiteManifest, tasks: &[SuiteTask]) -> Vec<String> {
    let mut blockers = Vec::new();
    if tasks.len() < TARGET_N {
        blockers.push(format!("n_tasks={} < {TARGET_N}", tasks.len()));
    }
    if !manifest.review.heterogeneity_reviewed {
        blockers.push("heterogeneity not reviewed".into());
    }
    if !manifest.review.provenance_reviewed {
        blockers.push("provenance not reviewed".into());
    }
    if !manifest.review.executable_hidden_reviewed {
        blockers.push("executable hidden verification not reviewed".into());
    }
    let mut ids = BTreeSet::new();
    let mut languages = BTreeSet::new();
    let mut classes = BTreeSet::new();
    let mut sizes = BTreeSet::new();
    let mut recall = 0usize;
    for task in tasks {
        if !ids.insert(&task.id) {
            blockers.push(format!("duplicate suite id {}", task.id));
        }
        if let Some(reason) = standin_reason(task) {
            blockers.push(format!("{}: {reason}", task.id));
        }
        languages.insert(task.language.as_str());
        classes.insert(task.class.as_str());
        sizes.insert(task.size.as_str());
        if task.recall_pressure != "none" {
            recall += 1;
        }
    }
    if !tasks.is_empty() {
        if languages.len() < 3 {
            blockers.push(format!(
                "languages={} < 3 ({})",
                languages.len(),
                join_set(&languages)
            ));
        }
        for required in ["bug", "feature", "refactor", "test", "recall"] {
            if !classes.contains(required) {
                blockers.push(format!("missing class {required}"));
            }
        }
        for required in ["small", "medium", "large"] {
            if !sizes.contains(required) {
                blockers.push(format!("missing size {required}"));
            }
        }
        if recall * 10 < tasks.len() {
            blockers.push(format!(
                "recall-pressure tasks={recall} (<10% of {})",
                tasks.len()
            ));
        }
    }
    blockers
}

/// 一行题 / 无出处 / 无命令：拒绝进入冻结集。
pub fn standin_reason(task: &SuiteTask) -> Option<String> {
    if task.id.is_empty() || task.name.is_empty() {
        return Some("empty id or name".into());
    }
    if task.description.trim().len() < MIN_DESCRIPTION {
        return Some("description too short for a real coding task".into());
    }
    let seed_bytes: usize = task.seed.iter().map(|file| file.content.len()).sum();
    if task.seed.is_empty() || seed_bytes < MIN_SEED_BYTES {
        return Some("seed too small / empty".into());
    }
    if task.hidden_commands.is_empty() {
        return Some("no executable hidden command".into());
    }
    if task.hidden_commands.iter().any(|cmd| cmd.argv.is_empty()) {
        return Some("hidden command argv is empty".into());
    }
    if task.hidden_overlay.is_empty() {
        return Some("no hidden overlay (tests must not live in the model-visible seed)".into());
    }
    if task.expected_files.is_empty() {
        return Some("no expected_files (self-check oracle missing)".into());
    }
    if task.runtime != crate::harvest::RUNTIME
        && task
            .seed
            .iter()
            .any(|file| file.path.starts_with("tests/") || file.path.contains("/tests/"))
    {
        return Some("seed contains tests/; hidden tests belong in hidden_overlay".into());
    }
    let has_provenance = task
        .source
        .commit
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        || task
            .source
            .repo
            .as_deref()
            .is_some_and(|value| !value.is_empty());
    if !has_provenance {
        return Some("missing source repo/commit provenance".into());
    }
    for required in [
        ("class", valid_class(&task.class)),
        ("size", valid_size(&task.size)),
        ("recall_pressure", valid_recall(&task.recall_pressure)),
    ] {
        if !required.1 {
            return Some(format!("invalid {}", required.0));
        }
    }
    if task.language.trim().is_empty() || task.edit_shape.trim().is_empty() {
        return Some("language/edit_shape empty".into());
    }
    None
}

fn resolve_program(name: &str) -> String {
    if name == "python" {
        crate::harvest::python_bin()
    } else {
        name.to_string()
    }
}

pub fn run_hidden_command(root: &Path, spec: &SuiteHiddenCommand) -> HiddenCommandResult {
    if spec.argv.is_empty() {
        return HiddenCommandResult {
            argv: spec.argv.clone(),
            expect_exit: spec.expect_exit,
            passed: false,
            ..HiddenCommandResult::default()
        };
    }
    let timeout = Duration::from_millis(if spec.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        spec.timeout_ms.clamp(1, MAX_TIMEOUT_MS)
    });
    let program = resolve_program(&spec.argv[0]);
    let mut command = Command::new(&program);
    command
        .args(&spec.argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut command);
    if spec.argv.iter().any(|arg| arg == "cargo") {
        command.env("CARGO_TARGET_DIR", root.join("target"));
        command.env("CARGO_TERM_COLOR", "never");
        command.env("CARGO_INCREMENTAL", "0");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HiddenCommandResult {
                argv: spec.argv.clone(),
                expect_exit: spec.expect_exit,
                stderr: format!("spawn failed: {error}"),
                passed: false,
                ..HiddenCommandResult::default()
            };
        }
    };
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_handle = std::thread::spawn(move || drain_option(stdout_pipe));
    let stderr_handle = std::thread::spawn(move || drain_option(stderr_pipe));
    let started = Instant::now();
    let (timed_out, exit) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (false, status.code()),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break (true, None);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                return HiddenCommandResult {
                    argv: spec.argv.clone(),
                    expect_exit: spec.expect_exit,
                    stderr: format!("wait failed: {error}"),
                    passed: false,
                    ..HiddenCommandResult::default()
                };
            }
        }
    };
    let (stdout, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_handle.join().unwrap_or_default();
    let passed = !timed_out && exit == Some(spec.expect_exit);
    HiddenCommandResult {
        argv: spec.argv.clone(),
        expect_exit: spec.expect_exit,
        exit,
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        passed,
    }
}

pub fn render_suite(pack: &SuitePack) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{SUITE_SCHEMA} frozen={} n={}/{} blockers={}\n",
        pack.frozen(),
        pack.tasks.len(),
        pack.manifest.target_n,
        pack.blockers.len()
    ));
    out.push_str(&format!(
        "review heterogeneity={} provenance={} executable_hidden={}\n",
        pack.manifest.review.heterogeneity_reviewed,
        pack.manifest.review.provenance_reviewed,
        pack.manifest.review.executable_hidden_reviewed
    ));
    if !pack.manifest.review.notes.is_empty() {
        out.push_str(&format!("notes: {}\n", pack.manifest.review.notes));
    }
    for blocker in &pack.blockers {
        out.push_str(&format!("  blocker: {blocker}\n"));
    }
    let mut by_class: BTreeMap<&str, u32> = BTreeMap::new();
    let mut by_lang: BTreeMap<&str, u32> = BTreeMap::new();
    for task in &pack.tasks {
        *by_class.entry(&task.class).or_default() += 1;
        *by_lang.entry(&task.language).or_default() += 1;
    }
    if !by_class.is_empty() {
        out.push_str("classes:");
        for (class, count) in by_class {
            out.push_str(&format!(" {class}={count}"));
        }
        out.push('\n');
        out.push_str("languages:");
        for (lang, count) in by_lang {
            out.push_str(&format!(" {lang}={count}"));
        }
        out.push('\n');
    }
    for task in &pack.tasks {
        if task.runtime == crate::harvest::RUNTIME {
            continue;
        }
        out.push_str(&format!(
            "  {} [{} {} {} {}]\n",
            task.id, task.language, task.class, task.size, task.edit_shape
        ));
    }
    let swebench = pack
        .tasks
        .iter()
        .filter(|task| task.runtime == crate::harvest::RUNTIME)
        .count();
    if swebench > 0 {
        out.push_str(&format!(
            "  swebench-verified docker instances={swebench} ({})\n",
            crate::harvest::DATASET
        ));
    }
    out.push_str(
        "smoke FIXTURES are diagnostic only and do not count toward 300. do not collect acceptance cells.\n",
    );
    out
}

fn valid_class(value: &str) -> bool {
    matches!(value, "bug" | "feature" | "refactor" | "test" | "recall")
}

fn valid_size(value: &str) -> bool {
    matches!(value, "small" | "medium" | "large")
}

fn valid_recall(value: &str) -> bool {
    matches!(value, "none" | "notes-then-reuse" | "long-gap")
}

fn join_set(set: &BTreeSet<&str>) -> String {
    set.iter().copied().collect::<Vec<_>>().join(",")
}

/// Hidden argv 由套件作者写死，不是模型输入。清掉密钥，但保留完整
/// 工具链环境（MSVC `link.exe` 搜索、rustup、INCLUDE/LIB）。允许名单会把
/// 能编过工作区的 `cargo test` 弄挂。
fn scrub_env(command: &mut Command) {
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        if key.to_str().is_some_and(is_secret_env) {
            continue;
        }
        command.env(key, value);
    }
}

fn is_secret_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "API_KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "TOKEN",
        "CREDENTIAL",
        "PRIVATE_KEY",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn drain_option<R: Read>(pipe: Option<R>) -> (String, bool) {
    match pipe {
        Some(mut reader) => drain_capped(&mut reader),
        None => (String::new(), false),
    }
}

fn drain_capped<R: Read>(reader: &mut R) -> (String, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remain = STDOUT_CAP.saturating_sub(buf.len());
                if remain == 0 {
                    truncated = true;
                    continue;
                }
                let take = remain.min(n);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_argv() -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "exit 0".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 0".into()]
        }
    }

    fn fail_argv() -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "exit 7".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 7".into()]
        }
    }

    fn sample_task(id: &str) -> SuiteTask {
        SuiteTask {
            id: id.into(),
            name: "harvested sample".into(),
            class: "bug".into(),
            language: "python".into(),
            size: "small".into(),
            edit_shape: "single-hunk".into(),
            recall_pressure: "none".into(),
            source: SuiteSource {
                kind: "git".into(),
                repo: Some("https://example.invalid/repo".into()),
                commit: Some("deadbeef".into()),
                path: Some("src/util.py".into()),
                note: Some("test-only, not a suite member".into()),
            },
            description: "A real harvested task description must be long enough to explain the bug, the expected edit, and the hidden command that will score it.".into(),
            extra_live_turns: Vec::new(),
            seed: vec![SuiteSeedFile {
                path: "src/util.py".into(),
                content: "\"\"\"Visit every item in order. The off-by-one below is the bug.\"\"\"\ndef visit_all(items):\n    out = []\n    for i in range(len(items)):\n        out.append(items[i + 1])\n    return out\n".into(),
            }],
            hidden_overlay: vec![SuiteSeedFile {
                path: "tests/test_util.py".into(),
                content: "from src.util import visit_all\n\ndef test_visit_all():\n    assert visit_all([1, 2]) == [1, 2]\n".into(),
            }],
            expected_files: vec![SuiteSeedFile {
                path: "src/util.py".into(),
                content: "def visit_all(items):\n    return list(items)\n".into(),
            }],
            hidden_files: Vec::new(),
            hidden_commands: vec![SuiteHiddenCommand {
                argv: ok_argv(),
                timeout_ms: 5_000,
                expect_exit: 0,
            }],
            runtime: String::new(),
        }
    }

    #[test]
    fn file_only_workspace_is_a_git_repo_so_status_works() {
        let dir = tempfile::tempdir().unwrap();
        let task = sample_task("file-git");
        materialize_live_workspace(&task, dir.path()).unwrap();
        assert!(
            dir.path().join(".git").is_dir(),
            "file-only seed must become a git repo"
        );
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git.status must succeed after seed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "seed commit must be clean, got {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn existing_git_dir_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        ensure_workspace_git(dir.path()).unwrap();
        let head = fs::read_to_string(dir.path().join(".git").join("HEAD")).unwrap();
        assert_eq!(head, "ref: refs/heads/main\n");
    }

    #[test]
    fn shipped_pack_is_frozen_and_suite_const_matches() {
        let pack = load_pack().expect("suite pack must load");
        assert!(pack.frozen());
        assert_eq!(pack.tasks.len(), 509, "{:?}", pack.tasks.len());
        assert!(pack.blockers.is_empty(), "{:?}", pack.blockers);
        const {
            assert!(crate::analysis::SUITE_FROZEN);
        }
        let rendered = render_suite(&pack);
        assert!(rendered.contains("frozen=true"));
        assert!(rendered.contains("n=509/300"));
        assert!(rendered.contains("heterogeneity=true"));
        assert!(rendered.contains("provenance=true"));
        assert!(rendered.contains("executable_hidden=true"));
        assert!(rendered.contains("openai-wire-tool-names"));
        assert!(rendered.contains("python-itertools-batched"));
        assert!(rendered.contains("js-ms-negative-parse"));
        assert!(rendered.contains("swebench-verified docker instances=500"));
        assert_eq!(TARGET_N, crate::analysis::MIN_TASKS);
        for task in &pack.tasks {
            assert!(
                standin_reason(task).is_none(),
                "{}: {:?}",
                task.id,
                standin_reason(task)
            );
            assert!(
                task.seed
                    .iter()
                    .all(|file| !file.path.starts_with("tests/")),
                "{} leaked tests into seed",
                task.id
            );
        }
    }

    #[test]
    fn live_turns_start_with_the_description() {
        let pack = load_pack().unwrap();
        let recall = pack
            .tasks
            .iter()
            .find(|task| task.id == "python-pep616-removeprefix")
            .unwrap();
        let turns = live_turns(recall);
        assert_eq!(turns[0], recall.description);
        assert_eq!(turns.len(), 1 + recall.extra_live_turns.len());
        assert!(turns.len() > 1);
        let simple = pack
            .tasks
            .iter()
            .find(|task| task.id == "python-itertools-batched")
            .unwrap();
        assert_eq!(live_turns(simple).len(), 1);
    }

    #[test]
    fn one_line_standin_is_rejected() {
        let mut task = sample_task("tiny");
        task.description = "fix it".into();
        task.seed = vec![SuiteSeedFile {
            path: "a.py".into(),
            content: "x=1\n".into(),
        }];
        task.hidden_commands.clear();
        task.source.commit = None;
        task.source.repo = None;
        let reason = standin_reason(&task).expect("stand-in");
        assert!(
            reason.contains("description")
                || reason.contains("seed")
                || reason.contains("command")
                || reason.contains("provenance"),
            "{reason}"
        );
    }

    #[test]
    fn hidden_command_runner_records_exit() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run_hidden_command(
            dir.path(),
            &SuiteHiddenCommand {
                argv: ok_argv(),
                timeout_ms: 5_000,
                expect_exit: 0,
            },
        );
        assert!(ok.passed, "{ok:?}");
        assert_eq!(ok.exit, Some(0));
        let bad = run_hidden_command(
            dir.path(),
            &SuiteHiddenCommand {
                argv: fail_argv(),
                timeout_ms: 5_000,
                expect_exit: 0,
            },
        );
        assert!(!bad.passed);
        assert_eq!(bad.exit, Some(7));
        assert!(!bad.timed_out);
    }

    #[test]
    fn frozen_manifest_with_blockers_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("manifest.json"),
            r#"{
  "schema": "agent-eval.suite.v1",
  "frozen": true,
  "target_n": 300,
  "review": {
    "heterogeneity_reviewed": false,
    "provenance_reviewed": false,
    "executable_hidden_reviewed": false
  }
}"#,
        )
        .unwrap();
        let error = load_pack_from(dir.path()).unwrap_err().to_string();
        assert!(error.contains("manifest.frozen=true"), "{error}");
    }

    #[test]
    fn harvested_shape_is_not_a_standin() {
        let reason = standin_reason(&sample_task("ok"));
        assert!(reason.is_none(), "{reason:?}");
        let mut leaked = sample_task("leaked");
        leaked.seed.push(SuiteSeedFile {
            path: "tests/hidden.py".into(),
            content: "assert False\n".into(),
        });
        let reason = standin_reason(&leaked).expect("leaked tests");
        assert!(reason.contains("tests/"), "{reason}");
    }

    #[test]
    fn harvested_seed_fails_and_expected_passes() {
        let pack = load_pack().expect("suite pack must load");
        let line = check_file_harvest(&pack).expect("file harvest self-check");
        assert_eq!(line, "file-harvested self-check ok n=9");
    }
}

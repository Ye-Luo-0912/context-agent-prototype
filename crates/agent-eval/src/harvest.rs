//! SWE-bench Verified 导入。300 题用官方数据集 + Docker 评测，不再手写最小化 crate。
//!
//! JSONL 只存 instance 元数据（issue 文本、FAIL_TO_PASS、commit），金补丁仍以
//! `princeton-nlp/SWE-bench_Verified` 为准。单元测试不拉镜像；gold 评测走
//! `AGENT_EVAL_SWEBENCH_DOCKER=1` / `--swebench-gold`。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::suite::{SuiteHiddenCommand, SuiteSeedFile, SuiteSource, SuiteTask};
use crate::workload::HiddenCommandResult;

pub const DATASET: &str = "princeton-nlp/SWE-bench_Verified";
pub const RUNTIME: &str = "swebench-docker";
/// 最小仓库的 Verified 实例，用作 opt-in gold 冒烟，不拉 500 张镜像。
pub const GOLD_SMOKE_INSTANCE: &str = "pallets__flask-5014";
const RECALL_EVERY: usize = 10;
const GOLD_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const LOG_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwebenchRow {
    pub instance_id: String,
    pub repo: String,
    pub base_commit: String,
    #[serde(default)]
    pub difficulty: String,
    #[serde(default)]
    pub version: String,
    pub problem_statement: String,
    pub fail_to_pass: String,
    #[serde(default)]
    pub patch_bytes: usize,
    #[serde(default)]
    pub test_patch_bytes: usize,
}

pub fn load_swebench_jsonl(path: &std::path::Path) -> anyhow::Result<Vec<SuiteTask>> {
    let text = std::fs::read_to_string(path)?;
    let mut tasks = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: SwebenchRow = serde_json::from_str(line)?;
        tasks.push(row_to_task(row, index));
    }
    Ok(tasks)
}

pub fn row_to_task(row: SwebenchRow, index: usize) -> SuiteTask {
    let recall = index.is_multiple_of(RECALL_EVERY);
    let description = row.problem_statement.replace('\r', " ");
    let instance = serde_json::json!({
        "dataset": DATASET,
        "instance_id": row.instance_id,
        "repo": row.repo,
        "base_commit": row.base_commit,
        "difficulty": row.difficulty,
        "fail_to_pass": row.fail_to_pass,
        "patch_bytes": row.patch_bytes,
        "test_patch_bytes": row.test_patch_bytes,
    });
    let oracle = serde_json::json!({
        "dataset": DATASET,
        "instance_id": row.instance_id,
        "oracle": "gold_patch",
    });
    SuiteTask {
        id: format!("swebench-{}", row.instance_id),
        name: row.instance_id.clone(),
        class: "bug".into(),
        language: "python".into(),
        size: size_from_difficulty(&row.difficulty),
        edit_shape: "swebench-issue".into(),
        recall_pressure: if recall {
            "notes-then-reuse".into()
        } else {
            "none".into()
        },
        source: SuiteSource {
            kind: "swebench-verified".into(),
            repo: Some(format!("https://github.com/{}", row.repo)),
            commit: Some(row.base_commit.clone()),
            path: Some(row.instance_id.clone()),
            note: Some(format!(
                "{DATASET} docker eval; gold patch stays in the dataset"
            )),
        },
        description,
        extra_live_turns: if recall {
            vec![
                "Write notes.md with three unrelated observations about logging. Do not start the issue fix.".into(),
                "Add another note about calendar formatting. Still do not edit project sources.".into(),
                "Using the repository in the evaluation container, resolve the GitHub issue.".into(),
            ]
        } else {
            Vec::new()
        },
        seed: vec![SuiteSeedFile {
            path: "instance.json".into(),
            content: instance.to_string(),
        }],
        hidden_overlay: vec![SuiteSeedFile {
            path: "fail_to_pass.json".into(),
            content: row.fail_to_pass.clone(),
        }],
        expected_files: vec![SuiteSeedFile {
            path: "oracle.json".into(),
            content: oracle.to_string(),
        }],
        hidden_files: Vec::new(),
        hidden_commands: vec![SuiteHiddenCommand {
            argv: vec![
                "swebench-docker".into(),
                "eval".into(),
                row.instance_id.clone(),
            ],
            timeout_ms: 120_000,
            expect_exit: 0,
        }],
        runtime: RUNTIME.into(),
    }
}

fn size_from_difficulty(difficulty: &str) -> String {
    match difficulty {
        "<15 min fix" => "small".into(),
        "15 min - 1 hour" => "medium".into(),
        _ => "large".into(),
    }
}

pub fn docker_opt_in() -> bool {
    matches!(
        std::env::var("AGENT_EVAL_SWEBENCH_DOCKER")
            .unwrap_or_default()
            .as_str(),
        "1" | "true" | "TRUE"
    )
}

pub fn instance_id_from_suite_id(suite_id: &str) -> Option<&str> {
    suite_id.strip_prefix("swebench-")
}

fn python_resolution()
-> &'static Result<tool_runtime::PythonInterpreter, tool_runtime::PythonInterpreterError> {
    static PYTHON: OnceLock<
        Result<tool_runtime::PythonInterpreter, tool_runtime::PythonInterpreterError>,
    > = OnceLock::new();
    PYTHON.get_or_init(|| {
        for variable in ["AGENT_EVAL_PYTHON", tool_runtime::PYTHON_EXECUTABLE_ENV] {
            if let Some(configured) = crate::envfile::get(variable) {
                return tool_runtime::resolve_python_interpreter_value(variable, configured.into());
            }
        }
        tool_runtime::resolve_python_interpreter(&[
            "AGENT_EVAL_PYTHON",
            tool_runtime::PYTHON_EXECUTABLE_ENV,
        ])
    })
}

pub(crate) fn python_interpreter() -> anyhow::Result<&'static tool_runtime::PythonInterpreter> {
    match python_resolution() {
        Ok(interpreter) => Ok(interpreter),
        Err(error) => Err(anyhow::Error::new(error.clone())),
    }
}

pub(crate) fn resolved_python_interpreter()
-> Result<tool_runtime::PythonInterpreter, tool_runtime::PythonInterpreterError> {
    python_resolution().clone()
}

fn gold_launcher() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/run_swebench_gold.py")
}

fn namespace() -> Option<String> {
    match std::env::var("AGENT_EVAL_SWEBENCH_NAMESPACE") {
        Ok(value) if value == "-" || value.is_empty() => None,
        Ok(value) => Some(value),
        Err(_) => Some("swebench".into()),
    }
}

pub fn gold_eval_argv(instance_id: &str) -> anyhow::Result<Vec<String>> {
    Ok(harness_eval_argv_with_program(
        python_interpreter()?.executable(),
        instance_id,
        "gold",
        &format!("agent-eval-gold-{instance_id}"),
    ))
}

pub fn harness_eval_argv(
    instance_id: &str,
    predictions_path: &str,
    run_id: &str,
) -> anyhow::Result<Vec<String>> {
    Ok(harness_eval_argv_with_program(
        python_interpreter()?.executable(),
        instance_id,
        predictions_path,
        run_id,
    ))
}

fn harness_eval_argv_with_program(
    python_program: &str,
    instance_id: &str,
    predictions_path: &str,
    run_id: &str,
) -> Vec<String> {
    let mut argv = std::iter::once(python_program.to_string())
        .chain([
            gold_launcher().to_string_lossy().into_owned(),
            "--dataset_name".into(),
            DATASET.into(),
            "--predictions_path".into(),
            predictions_path.into(),
            "--max_workers".into(),
            "1".into(),
            "--instance_ids".into(),
            instance_id.into(),
            "--run_id".into(),
            run_id.into(),
            "--cache_level".into(),
            "env".into(),
        ])
        .collect::<Vec<_>>();
    if let Some(ns) = namespace() {
        argv.push("--namespace".into());
        argv.push(ns);
    }
    argv
}

pub fn gold_work_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/swebench-eval")
}

/// 对单题跑官方 harness 的 gold patch。默认关闭；单元测试不得调用。
pub fn run_gold_eval(instance_id: &str) -> anyhow::Result<HiddenCommandResult> {
    refuse_instance_id(instance_id)?;
    let work = gold_work_dir();
    std::fs::create_dir_all(&work)?;
    let run_id = format!("agent-eval-gold-{instance_id}");
    clear_eval_logs(&work, &run_id);
    run_harness(&work, &gold_eval_argv(instance_id)?, instance_id)
}

/// 用模型产出的 git diff 跑官方 harness。默认关闭；单元测试不得调用。
pub fn run_prediction_eval(
    instance_id: &str,
    model_patch: &str,
    run_id: &str,
) -> anyhow::Result<HiddenCommandResult> {
    refuse_instance_id(instance_id)?;
    refuse_run_id(run_id)?;
    if model_patch.trim().is_empty() {
        // 空补丁会被官方 harness 直接丢掉（No instances to run），不必再打 GitHub/HF。
        return Ok(HiddenCommandResult {
            argv: vec!["swebench-docker".into(), "eval".into(), instance_id.into()],
            stderr: "skipped swebench docker: empty model patch".into(),
            passed: false,
            ..HiddenCommandResult::default()
        });
    }
    let work = gold_work_dir();
    std::fs::create_dir_all(&work)?;
    let pred_path = work.join(format!("{run_id}.jsonl"));
    let row = serde_json::json!({
        "instance_id": instance_id,
        "model_name_or_path": "agent-eval-pilot",
        "model_patch": model_patch.replace('\r', ""),
    });
    std::fs::write(&pred_path, format!("{row}\n"))?;
    clear_eval_logs(&work, run_id);
    run_harness(
        &work,
        &harness_eval_argv(instance_id, &pred_path.to_string_lossy(), run_id)?,
        instance_id,
    )
}

fn refuse_instance_id(instance_id: &str) -> anyhow::Result<()> {
    if instance_id.is_empty()
        || instance_id
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
    {
        anyhow::bail!("refusing swebench instance id {instance_id:?}");
    }
    Ok(())
}

fn refuse_run_id(run_id: &str) -> anyhow::Result<()> {
    if run_id.is_empty()
        || run_id.len() > 120
        || run_id
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
    {
        anyhow::bail!("refusing swebench run id {run_id:?}");
    }
    Ok(())
}

fn clear_eval_logs(work: &Path, run_id: &str) {
    let log_dir = work.join("logs/run_evaluation").join(run_id);
    let _ = std::fs::remove_dir_all(&log_dir);
}

fn run_harness(
    work: &Path,
    argv: &[String],
    instance_id: &str,
) -> anyhow::Result<HiddenCommandResult> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("PYTHONPATH");
    let mut child = cmd
        .spawn()
        .map_err(|error| anyhow::anyhow!("spawn swebench harness ({:?}): {error}", argv[0]))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || read_capped(stdout_pipe, LOG_CAP));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr_pipe, LOG_CAP));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= GOLD_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_thread.join().unwrap_or_default();
                let stderr = stderr_thread.join().unwrap_or_default();
                return Ok(HiddenCommandResult {
                    argv: argv.to_vec(),
                    expect_exit: 0,
                    timed_out: true,
                    stdout,
                    stderr,
                    passed: false,
                    ..HiddenCommandResult::default()
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => anyhow::bail!("wait swebench harness: {error}"),
        }
    };
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    let resolved = gold_resolved(work, instance_id, &stdout, &stderr);
    Ok(HiddenCommandResult {
        argv: argv.to_vec(),
        expect_exit: 0,
        exit: status.code(),
        timed_out: false,
        stdout,
        stderr,
        passed: status.success() && resolved,
        ..HiddenCommandResult::default()
    })
}

pub fn clone_opt_in() -> bool {
    matches!(
        std::env::var("AGENT_EVAL_SWEBENCH_CLONE")
            .unwrap_or_default()
            .as_str(),
        "1" | "true" | "TRUE"
    )
}

pub fn checkout_dir(instance_id: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/swebench-src")
        .join(instance_id)
}

/// 浅拉 `base_commit` 到 `target/swebench-src/<instance>`。单元测试不得调用。
pub fn ensure_checkout(repo_url: &str, commit: &str, instance_id: &str) -> anyhow::Result<PathBuf> {
    refuse_instance_id(instance_id)?;
    refuse_github_url(repo_url)?;
    refuse_commit(commit)?;
    let dest = checkout_dir(instance_id);
    if dest.join(".git").is_dir() && commit_matches(&dest, commit) {
        return Ok(dest);
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(&dest)?;
    git_checked(&["init"], &dest)?;
    git_checked(&["remote", "add", "origin", repo_url], &dest)?;
    if git_checked(&["fetch", "--depth", "1", "origin", commit], &dest).is_err() {
        git_checked(&["fetch", "origin", commit], &dest)?;
    }
    git_checked(&["checkout", "--force", "FETCH_HEAD"], &dest)?;
    Ok(dest)
}

pub fn clone_engine_workspace(cache: &Path, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let status = Command::new("git")
        .args([
            "clone",
            cache
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-utf8 checkout path"))?,
            dest.to_str()
                .ok_or_else(|| anyhow::anyhow!("non-utf8 dest path"))?,
        ])
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("git clone from checkout cache failed with {status}");
    }
    Ok(())
}

/// 相对 HEAD 的模型补丁（含未跟踪文件）。空 diff 仍交给 harness，ITT 记失败。
/// `.focus-agent/` 是运行时目录，不得进入 SWE-bench prediction。
pub fn git_model_patch(workspace: &Path) -> anyhow::Result<String> {
    exclude_runtime_dir(workspace)?;
    let _ = Command::new("git")
        .args(["reset", "-q", "--", ".focus-agent"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .status();
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .status()?;
    if !add.success() {
        anyhow::bail!("git add -A failed with {add}");
    }
    let output = Command::new("git")
        .args(["diff", "--cached", "HEAD"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git diff --cached HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let patch = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    if patch.contains(".focus-agent/") || patch.contains(".focus-agent\\") {
        anyhow::bail!("model patch leaked .focus-agent runtime files");
    }
    Ok(patch)
}

fn exclude_runtime_dir(workspace: &Path) -> anyhow::Result<()> {
    let path = workspace.join(".git").join("info").join("exclude");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    let already = body.lines().any(|line| {
        let line = line.trim();
        line == ".focus-agent" || line == ".focus-agent/"
    });
    if already {
        return Ok(());
    }
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(".focus-agent/\n");
    fs::write(path, body)?;
    Ok(())
}

fn refuse_github_url(url: &str) -> anyhow::Result<()> {
    let rest = url.strip_prefix("https://github.com/").ok_or_else(|| {
        anyhow::anyhow!("swebench clone URL must be https://github.com/owner/repo")
    })?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let owner = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    if parts.next().is_some()
        || owner.is_empty()
        || name.is_empty()
        || !owner
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        anyhow::bail!("refusing swebench repo URL {url:?}");
    }
    Ok(())
}

fn refuse_commit(commit: &str) -> anyhow::Result<()> {
    if commit.len() < 7 || commit.len() > 40 || !commit.chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("refusing swebench commit {commit:?}");
    }
    Ok(())
}

fn commit_matches(repo: &Path, commit: &str) -> bool {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let head = String::from_utf8_lossy(&output.stdout);
    let head = head.trim();
    head == commit || (commit.len() >= 7 && head.starts_with(commit))
}

fn git_checked(args: &[&str], cwd: &Path) -> anyhow::Result<()> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| anyhow::anyhow!("spawn git {args:?}: {error}"))?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || read_capped(stdout_pipe, LOG_CAP));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr_pipe, LOG_CAP));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= Duration::from_secs(15 * 60) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("git {args:?} timed out");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => anyhow::bail!("wait git {args:?}: {error}"),
        }
    };
    let _stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        anyhow::bail!("git {args:?} failed with {status}: {stderr}");
    }
    Ok(())
}

fn read_capped<R: Read>(source: Option<R>, cap: usize) -> String {
    let Some(mut source) = source else {
        return String::new();
    };
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match source.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&tmp[..take]);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn gold_resolved(work: &Path, instance_id: &str, stdout: &str, stderr: &str) -> bool {
    let mut reports = Vec::new();
    collect_json(work, &mut reports);
    for path in reports {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if report_marks_resolved(&value, instance_id) {
            return true;
        }
    }
    let blob = format!("{stdout}\n{stderr}").to_lowercase();
    blob.contains(&instance_id.to_lowercase())
        && (blob.contains("instances resolved: 1") || blob.contains("\"resolved\": 1"))
}

fn report_marks_resolved(value: &serde_json::Value, instance_id: &str) -> bool {
    if let Some(ids) = value.get("resolved_ids").and_then(|row| row.as_array())
        && ids.iter().any(|id| id.as_str() == Some(instance_id))
    {
        return true;
    }
    if value
        .get("resolved")
        .and_then(|row| row.as_array())
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(instance_id)))
    {
        return true;
    }
    if value
        .get(instance_id)
        .and_then(|row| row.get("resolved"))
        .and_then(|row| row.as_bool())
        == Some(true)
    {
        return true;
    }
    false
}

fn collect_json(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json(&path, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::standin_reason;

    #[test]
    fn first_verified_row_is_not_a_standin() {
        let row = SwebenchRow {
            instance_id: "astropy__astropy-12907".into(),
            repo: "astropy/astropy".into(),
            base_commit: "d16bfe05a744909de4b27f5875fe0d4ed41ce607".into(),
            difficulty: "15 min - 1 hour".into(),
            version: "4.3".into(),
            problem_statement: "Modeling's separability_matrix does not compute nested CompoundModels correctly. Nested compounds should keep independent linear inputs separable.".into(),
            fail_to_pass: "[\"test_separable\"]".into(),
            patch_bytes: 470,
            test_patch_bytes: 1415,
        };
        let task = row_to_task(row, 0);
        assert_eq!(task.runtime, RUNTIME);
        assert_eq!(task.recall_pressure, "notes-then-reuse");
        assert_eq!(task.size, "medium");
        assert!(
            standin_reason(&task).is_none(),
            "{:?}",
            standin_reason(&task)
        );
    }

    #[test]
    fn gold_eval_argv_is_official_harness_gold() {
        let argv = harness_eval_argv_with_program(
            "/resolved/python",
            GOLD_SMOKE_INSTANCE,
            "gold",
            &format!("agent-eval-gold-{GOLD_SMOKE_INSTANCE}"),
        );
        assert_eq!(argv[0], "/resolved/python");
        assert!(argv.iter().any(|row| row.ends_with("run_swebench_gold.py")));
        assert!(argv.contains(&"gold".into()));
        assert!(argv.contains(&DATASET.into()));
        assert!(argv.contains(&GOLD_SMOKE_INSTANCE.into()));
    }

    #[test]
    fn prediction_argv_points_at_a_jsonl_not_gold() {
        let argv = harness_eval_argv_with_program(
            "/resolved/python",
            GOLD_SMOKE_INSTANCE,
            "pred.jsonl",
            "agent-eval-pred-flask-append-1",
        );
        assert_eq!(argv[0], "/resolved/python");
        assert!(argv.contains(&"pred.jsonl".into()));
        assert!(!argv.contains(&"gold".into()));
        assert!(argv.contains(&"agent-eval-pred-flask-append-1".into()));
    }

    #[test]
    fn empty_patch_skips_the_harness() {
        let result =
            run_prediction_eval(GOLD_SMOKE_INSTANCE, "  \n", "agent-eval-empty-patch").unwrap();
        assert!(!result.passed);
        assert!(result.stderr.contains("empty model patch"));
        assert_eq!(result.argv[0], "swebench-docker");
    }

    #[test]
    fn git_model_patch_omits_runtime_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_in(root, &["init", "-b", "main"]);
        git_in(root, &["config", "user.email", "eval@test"]);
        git_in(root, &["config", "user.name", "eval"]);
        std::fs::write(root.join("app.py"), "print(1)\n").unwrap();
        git_in(root, &["add", "app.py"]);
        git_in(root, &["commit", "-m", "seed"]);
        std::fs::write(root.join("app.py"), "print(2)\n").unwrap();
        std::fs::create_dir_all(root.join(".focus-agent/authority")).unwrap();
        std::fs::write(root.join(".focus-agent/authority/secret.jsonl"), "lease\n").unwrap();
        let patch = git_model_patch(root).unwrap();
        assert!(patch.contains("print(2)"), "{patch}");
        assert!(
            !patch.contains(".focus-agent"),
            "runtime dir leaked into prediction:\n{patch}"
        );
        assert!(!patch.contains("secret.jsonl"), "{patch}");
    }

    fn git_in(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    #[test]
    fn clone_url_and_commit_are_fail_closed() {
        assert!(refuse_github_url("https://github.com/pallets/flask").is_ok());
        assert!(refuse_github_url("https://evil.example/pallets/flask").is_err());
        assert!(refuse_github_url("https://github.com/pallets/flask/extra").is_err());
        assert!(refuse_commit("d16bfe05a744909de4b27f5875fe0d4ed41ce607").is_ok());
        assert!(refuse_commit("../etc/passwd").is_err());
        assert!(refuse_run_id("agent-eval-pred-flask-append-1").is_ok());
        assert!(refuse_run_id("foo;rm").is_err());
    }

    #[test]
    fn gold_report_detects_resolved_ids() {
        let value = serde_json::json!({
            "resolved_ids": [GOLD_SMOKE_INSTANCE],
            "unresolved_ids": []
        });
        assert!(report_marks_resolved(&value, GOLD_SMOKE_INSTANCE));
        assert!(!report_marks_resolved(&value, "django__django-1"));
    }
}

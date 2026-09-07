//! F6 live product walkthroughs: the real composition root plus a real
//! provider, without the TUI. Ignored in ordinary CI; run explicitly when
//! `eval.env` / `OPENAI_API_KEY` is present.
//!
//! Approval is `PolicyApprovalGate::permissive()` so an unattended session
//! can write. That is the only product-policy difference from the TUI
//! interactive gate. Results are a walkthrough record, not a model-quality
//! score.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, ModelSelection, build_context_engine,
    compose, try_model_from_env,
};
use agent_contracts::{
    RuntimeEvent, RuntimeFailureClass, ToolSurfaceDemand, ToolSurfaceRequirement,
};
use agent_core::PolicyApprovalGate;
use agent_runtime::{RuntimeHandle, TaskStatus};
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use tokio::time::Instant;
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

const FULL_TURN: Duration = Duration::from_secs(12 * 60);
const INTERRUPT_ROUNDS: usize = 2;
const FULL_ROUNDS: usize = 12;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn load_eval_env() {
    let candidates = [
        std::env::var("EVAL_ENV_FILE").ok().map(PathBuf::from),
        Some(PathBuf::from("eval.env")),
        Some(PathBuf::from(".env")),
        Some(repo_root().join("eval.env")),
        Some(repo_root().join(".env")),
    ];
    let Some(path) = candidates.into_iter().flatten().find(|path| path.is_file()) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    const ALLOWED: &[&str] = &[
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_MODEL",
        "OPENAI_API_PROTOCOL",
        "OPENAI_CONTEXT_WINDOW",
        "OPENAI_MAX_OUTPUT_TOKENS",
        "OPENAI_TEMPERATURE",
        "AGENT_PYTHON",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
    ];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !ALLOWED.contains(&key) {
            continue;
        }
        if std::env::var(key)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        {
            continue;
        }
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if value.trim().is_empty() {
            continue;
        }
        // Process-level provider config for this walkthrough only.
        unsafe { std::env::set_var(key, value) };
    }
}

fn report_path() -> PathBuf {
    std::env::var("LIVE_WALK_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("docs/walkthroughs/2026-09-06-f6.md"))
}

#[derive(Default)]
struct TurnTrace {
    tools: Vec<String>,
    unique_tools: BTreeSet<String>,
    failures: Vec<String>,
    last_assistant: String,
    plan_updated: bool,
    task_completed: bool,
    round_budget: bool,
    turn_completed: bool,
    model_rounds: usize,
    grep_partial: bool,
    ranged_reads: usize,
    timed_out: bool,
}

#[derive(Default)]
struct TaskRecord {
    name: &'static str,
    goal: String,
    runtime_ok: bool,
    stop: String,
    elapsed_ms: u64,
    tools: Vec<String>,
    files_changed: Vec<String>,
    plan: Vec<String>,
    next_action: String,
    harness: Vec<String>,
    failures: Vec<String>,
    notes: Vec<String>,
    last_assistant: String,
}

async fn product_compose(
    root: &Path,
    max_tool_rounds: Option<usize>,
) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = Workspace::open(root).await?;
    let model = match try_model_from_env()? {
        ModelSelection::Mock(_) => {
            anyhow::bail!("AGENT_DEMO is set; F6 live walk needs a real provider")
        }
        ModelSelection::Provider(transport, profile) => {
            eprintln!("{}", profile.banner());
            transport
        }
    };
    let context_engine = build_context_engine(
        ContextPolicy::Dynamic,
        workspace.state_dir(),
        Some(model.clone()),
    )
    .await?;
    let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);
    let recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
    let has_recipes = !recipes.is_empty();
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let base_tools = Arc::new(BuiltinToolDispatcher::with_config_and_verification_recipes(
        workspace.clone(),
        Default::default(),
        (*recipes).clone(),
    ));
    let artifact_store = Arc::new(workspace.clone());
    let output_broker = Arc::new(WorkspaceOutputBroker::new(artifact_store.clone()));
    let reservation_journal = workspace
        .state_dir()
        .join("authority")
        .join("broker-reservations.jsonl");
    compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace,
        context_engine,
        model,
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(artifact_store),
        output_broker: Some(output_broker),
        max_tool_rounds,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation_journal),
        verification_recipes: Some(recipes),
        project_proof_refresh: has_recipes,
        // Harness compositions never arm Unix host-death containment (no
        // watchdog dispatch in this executable).
        host_death_watchdog: false,
        // Harness/eval compositions register no external capabilities by default.
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await
}

async fn start_work(handle: &RuntimeHandle, goal: &str) -> anyhow::Result<()> {
    handle.set_focus(goal.to_string()).await?;
    let tasks = handle.list_tasks().await?;
    let task = tasks
        .iter()
        .find(|task| task.goal == goal && matches!(task.status, TaskStatus::Active))
        .ok_or_else(|| anyhow::anyhow!("set_focus did not create an active task"))?;
    if task.tool_requirement_count == 0 {
        handle
            .replace_task_tool_requirements(
                task.id,
                task.tool_requirement_revision,
                vec![ToolSurfaceRequirement {
                    tool_name: "task.manage".into(),
                    demand: ToolSurfaceDemand::PreferSurface,
                    reason: "long-task checklist".into(),
                }],
            )
            .await?;
    }
    handle.user_message(goal.to_string()).await?;
    Ok(())
}

fn observe(trace: &mut TurnTrace, event: &RuntimeEvent) {
    match event {
        RuntimeEvent::ToolStarted { call } => {
            trace.tools.push(call.name.clone());
            trace.unique_tools.insert(call.name.clone());
            eprintln!("F6 tool {}", call.name);
        }
        RuntimeEvent::ToolFinished { output, .. } => {
            if output.tool_name == "search.grep"
                && output
                    .metadata
                    .get("scan_incomplete")
                    .and_then(|value| value.as_bool())
                    == Some(true)
            {
                trace.grep_partial = true;
            }
            if output.tool_name == "fs.read" && output.file_line_range().is_some() {
                trace.ranged_reads += 1;
            }
        }
        RuntimeEvent::AssistantMessage { content } if !content.trim().is_empty() => {
            trace.last_assistant = content.chars().take(800).collect();
        }
        RuntimeEvent::TaskProgressUpdated { accepted: true, .. } => trace.plan_updated = true,
        RuntimeEvent::TaskCompleted { .. } => trace.task_completed = true,
        RuntimeEvent::TurnCompleted => trace.turn_completed = true,
        RuntimeEvent::ModelStarted { .. } => trace.model_rounds += 1,
        RuntimeEvent::Failure {
            class,
            retryable,
            message,
        } => {
            trace
                .failures
                .push(format!("{class:?} retryable={retryable} {message}"));
            if matches!(class, RuntimeFailureClass::RoundBudget) {
                trace.round_budget = true;
            }
        }
        _ => {}
    }
}

async fn drain_turn(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    limit: Duration,
) -> TurnTrace {
    let mut trace = TurnTrace::default();
    let deadline = Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            trace.timed_out = true;
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(envelope)) => {
                observe(&mut trace, &envelope.event);
                if trace.turn_completed || trace.round_budget {
                    break;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => break,
            Err(_) => {
                trace.timed_out = true;
                break;
            }
        }
    }
    trace
}

async fn continue_when_idle(handle: &RuntimeHandle) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match handle.continue_active_task().await {
            Ok(_) => return Ok(()),
            Err(error) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("continue_active_task never accepted: {error}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, body) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
}

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name();
            if name == ".focus-agent" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
                continue;
            }
            if let Ok(body) = std::fs::read_to_string(&path) {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative, body));
            }
        }
    }
    walk(root, root, &mut out);
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

fn changed_paths(before: &[(String, String)], after: &[(String, String)]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for (name, body) in after {
        match before.iter().find(|(existing, _)| existing == name) {
            Some((_, old)) if old == body => {}
            _ => {
                names.insert(name.clone());
            }
        }
    }
    names.into_iter().collect()
}

fn file_contains(root: &Path, relative: &str, needle: &str) -> bool {
    std::fs::read_to_string(root.join(relative))
        .map(|body| body.contains(needle))
        .unwrap_or(false)
}

fn plant_bug(root: &Path) {
    write_files(
        root,
        &[
            (
                "README.md",
                "Tiny calculator. `add` is wrong (subtracts). `mul` is correct.\n",
            ),
            (
                "calc.py",
                "def add(a, b):\n    return a - b  # BUG: currently subtracts\n\n\ndef mul(a, b):\n    return a * b\n",
            ),
        ],
    );
}

fn plant_feature(root: &Path) {
    write_files(
        root,
        &[
            (
                "README.md",
                "Add timeout_ms=5000 to config defaults and expose it from app.py as TIMEOUT_MS.\nDo not change host or port.\n",
            ),
            (
                "config.py",
                "DEFAULTS = {\"host\": \"127.0.0.1\", \"port\": 8080}\n\n\ndef get(key):\n    return DEFAULTS[key]\n",
            ),
            (
                "app.py",
                "from config import get\n\nHOST = get(\"host\")\nPORT = get(\"port\")\n",
            ),
        ],
    );
}

fn plant_refactor(root: &Path) {
    write_files(
        root,
        &[
            (
                "README.md",
                "Move the duplicated normalize() helper into names.py and import it from users.py and files.py.\nDo not change behavior.\n",
            ),
            (
                "users.py",
                "def normalize(name):\n    return name.strip().lower()\n\n\ndef display_user(name):\n    return normalize(name)\n",
            ),
            (
                "files.py",
                "def normalize(name):\n    return name.strip().lower()\n\n\ndef display_file(name):\n    return normalize(name)\n",
            ),
            ("names.py", "# Shared name helpers belong here.\n"),
        ],
    );
}

async fn read_plan(handle: &RuntimeHandle, record: &mut TaskRecord) {
    if let Ok(Some(view)) = handle.task_plan_view().await {
        record.plan = view.plan_progress;
        record.next_action = view.next_action;
    }
}

fn apply_trace(record: &mut TaskRecord, trace: &TurnTrace) {
    for name in &trace.unique_tools {
        if !record.tools.iter().any(|existing| existing == name) {
            record.tools.push(name.clone());
        }
    }
    record.tools.sort();
    record.failures.extend(trace.failures.clone());
    record.last_assistant = trace.last_assistant.clone();
    if trace.timed_out {
        record.stop = "timeout".into();
        record.notes.push("turn wait hit the wall clock cap".into());
    } else if trace.round_budget {
        record.stop = "round-budget".into();
    } else if trace.task_completed {
        record.stop = "task-completed".into();
    } else if trace.turn_completed {
        record.stop = "turn-completed".into();
    }
    if trace.plan_updated {
        record.notes.push("task.manage plan was accepted".into());
    }
    if trace.grep_partial {
        record
            .notes
            .push("search.grep reported PARTIAL/scan_incomplete".into());
    }
    if trace.ranged_reads > 0 {
        record.notes.push(format!(
            "fs.read stamped {} ranged window(s)",
            trace.ranged_reads
        ));
    }
    record
        .notes
        .push(format!("model rounds observed: {}", trace.model_rounds));
}

async fn run_refactor_interrupt(root: &Path, record: &mut TaskRecord) -> anyhow::Result<()> {
    let checkpoint_path = root
        .join(".focus-agent")
        .join("checkpoints")
        .join("f6.json");
    let composed = product_compose(root, Some(INTERRUPT_ROUNDS)).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let handle = composed.handle().clone();
    let goal = record.goal.clone();
    start_work(&handle, &goal).await?;
    let started = Instant::now();
    let trace = drain_turn(&mut events, FULL_TURN).await;
    record.elapsed_ms += started.elapsed().as_millis() as u64;
    apply_trace(record, &trace);
    read_plan(&handle, record).await;
    let goal_before = handle
        .list_tasks()
        .await?
        .into_iter()
        .find(|task| matches!(task.status, TaskStatus::Active))
        .map(|task| task.goal)
        .unwrap_or_default();
    record.notes.push(format!(
        "interrupt session goal={goal_before:?} round_budget={} turn_completed={}",
        trace.round_budget, trace.turn_completed
    ));
    let checkpoint = composed.instance.checkpoint().await?;
    std::fs::create_dir_all(checkpoint_path.parent().unwrap())?;
    std::fs::write(&checkpoint_path, serde_json::to_vec(&checkpoint)?)?;
    composed.shutdown().await?;

    let composed = product_compose(root, Some(FULL_ROUNDS)).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let bytes = std::fs::read(&checkpoint_path)?;
    let checkpoint: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&bytes)?;
    checkpoint.validate()?;
    composed.instance.restore(checkpoint).await?;
    let handle = composed.handle().clone();
    let restored_goal = handle
        .task_plan_view()
        .await?
        .map(|view| view.original_goal)
        .unwrap_or_default();
    record
        .notes
        .push(format!("restored original_goal={restored_goal:?}"));
    let started = Instant::now();
    continue_when_idle(&handle).await?;
    let trace = drain_turn(&mut events, FULL_TURN).await;
    record.elapsed_ms += started.elapsed().as_millis() as u64;
    apply_trace(record, &trace);
    read_plan(&handle, record).await;
    composed.shutdown().await?;
    record.runtime_ok = true;
    Ok(())
}

async fn run_one_turn(
    root: &Path,
    goal: &str,
    max_rounds: usize,
    record: &mut TaskRecord,
) -> anyhow::Result<TurnTrace> {
    let composed = product_compose(root, Some(max_rounds)).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let handle = composed.handle().clone();
    start_work(&handle, goal).await?;
    let started = Instant::now();
    let trace = drain_turn(&mut events, FULL_TURN).await;
    record.elapsed_ms += started.elapsed().as_millis() as u64;
    apply_trace(record, &trace);
    read_plan(&handle, record).await;
    record.runtime_ok = true;
    composed.shutdown().await?;
    Ok(trace)
}

fn render_report(provider: &str, records: &[TaskRecord]) -> String {
    let mut out = String::from("# F6 live walkthrough (no TUI)\n\n");
    out.push_str(&format!(
        "- Date: 2026-09-06\n- Entry: `agent-compose` product composition + `RuntimeHandle` (`/work` sequence)\n- TUI: not used (deferred)\n- Approval: `PolicyApprovalGate::permissive()` (unattended writes)\n- Provider: {provider}\n- Context policy: dynamic\n\n"
    ));
    out.push_str("This is a product walkthrough record, not a model-success score. Ordinary model misses stay transparent.\n\n");
    for record in records {
        out.push_str(&format!("## {}\n\n", record.name));
        out.push_str(&format!("- Goal: {}\n", record.goal));
        out.push_str(&format!("- Runtime ok: {}\n", record.runtime_ok));
        out.push_str(&format!("- Stop: {}\n", record.stop));
        out.push_str(&format!("- Elapsed: {} ms\n", record.elapsed_ms));
        out.push_str(&format!("- Tools: {}\n", record.tools.join(", ")));
        out.push_str(&format!(
            "- Files changed: {}\n",
            if record.files_changed.is_empty() {
                "-".into()
            } else {
                record.files_changed.join(", ")
            }
        ));
        if !record.plan.is_empty() {
            out.push_str("- Plan:\n");
            for line in &record.plan {
                out.push_str(&format!("  - {line}\n"));
            }
        }
        if !record.next_action.is_empty() {
            out.push_str(&format!("- Next action: {}\n", record.next_action));
        }
        out.push_str("- Harness checks:\n");
        for line in &record.harness {
            out.push_str(&format!("  - {line}\n"));
        }
        if !record.failures.is_empty() {
            out.push_str("- Failures:\n");
            for line in &record.failures {
                out.push_str(&format!("  - {line}\n"));
            }
        }
        if !record.notes.is_empty() {
            out.push_str("- Notes:\n");
            for line in &record.notes {
                out.push_str(&format!("  - {line}\n"));
            }
        }
        if !record.last_assistant.is_empty() {
            out.push_str("\nAssistant (truncated):\n\n```\n");
            out.push_str(&record.last_assistant);
            out.push_str("\n```\n");
        }
        out.push('\n');
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live provider walkthrough; needs eval.env / OPENAI_API_KEY"]
async fn f6_three_live_product_walkthroughs() {
    load_eval_env();
    let provider = match try_model_from_env() {
        Ok(ModelSelection::Provider(_, profile)) => profile.banner(),
        Ok(ModelSelection::Mock(_)) => panic!("AGENT_DEMO is set; refuse to treat mock as live"),
        Err(error) => panic!("provider not configured: {error}"),
    };

    let mut records = Vec::new();

    // 1. Fix a real small bug.
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        plant_bug(&root);
        let before = snapshot(&root);
        let mut record = TaskRecord {
            name: "1. fix a small bug",
            goal: "Fix calc.add so it returns a + b. Do not change mul. Use the workspace tools; keep the public function names.".into(),
            ..TaskRecord::default()
        };
        eprintln!("F6 starting {}", record.name);
        let goal = record.goal.clone();
        let result = run_one_turn(&root, &goal, FULL_ROUNDS, &mut record).await;
        if let Err(error) = result {
            record.runtime_ok = false;
            record.failures.push(error.to_string());
        }
        let after = snapshot(&root);
        record.files_changed = changed_paths(&before, &after);
        let add_body = std::fs::read_to_string(root.join("calc.py")).unwrap_or_default();
        let add_fixed = add_body.contains("return a + b") && !add_body.contains("return a - b");
        let mul_ok = add_body.contains("return a * b");
        record
            .harness
            .push(format!("calc.add returns a + b: {add_fixed}"));
        record
            .harness
            .push(format!("calc.mul still multiplies: {mul_ok}"));
        records.push(record);
    }

    // 2. Add a small feature across two files.
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        plant_feature(&root);
        let before = snapshot(&root);
        let mut record = TaskRecord {
            name: "2. add a small feature",
            goal: "Add timeout_ms=5000 to config.DEFAULTS and expose it from app.py as TIMEOUT_MS = get(\"timeout_ms\"). Do not change host or port.".into(),
            ..TaskRecord::default()
        };
        eprintln!("F6 starting {}", record.name);
        let goal = record.goal.clone();
        let result = run_one_turn(&root, &goal, FULL_ROUNDS, &mut record).await;
        if let Err(error) = result {
            record.runtime_ok = false;
            record.failures.push(error.to_string());
        }
        let after = snapshot(&root);
        record.files_changed = changed_paths(&before, &after);
        record.harness.push(format!(
            "config has timeout_ms: {}",
            file_contains(&root, "config.py", "timeout_ms")
        ));
        record.harness.push(format!(
            "app.py reads timeout_ms: {}",
            file_contains(&root, "app.py", "timeout_ms")
        ));
        record.harness.push(format!(
            "host/port still present: {}",
            file_contains(&root, "config.py", "127.0.0.1")
                && file_contains(&root, "config.py", "8080")
        ));
        records.push(record);
    }

    // 3. Cross-file refactor, interrupt at the round budget, restore, continue.
    {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        plant_refactor(&root);
        let before = snapshot(&root);
        let mut record = TaskRecord {
            name: "3. cross-file refactor with interrupt/continue",
            goal: "Move normalize() into names.py and import it from users.py and files.py. Do not change behavior.".into(),
            ..TaskRecord::default()
        };
        eprintln!("F6 starting {}", record.name);
        let result = run_refactor_interrupt(&root, &mut record).await;
        if let Err(error) = result {
            record.runtime_ok = false;
            record.failures.push(error.to_string());
        }
        let after = snapshot(&root);
        record.files_changed = changed_paths(&before, &after);
        record.harness.push(format!(
            "names.py defines normalize: {}",
            file_contains(&root, "names.py", "def normalize")
        ));
        record.harness.push(format!(
            "users.py imports names: {}",
            file_contains(&root, "users.py", "names")
        ));
        record.harness.push(format!(
            "files.py imports names: {}",
            file_contains(&root, "files.py", "names")
        ));
        records.push(record);
    }

    write_and_assert(&provider, &records);
}

fn write_and_assert(provider: &str, records: &[TaskRecord]) {
    let markdown = render_report(provider, records);
    let path = report_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, markdown).expect("write F6 walkthrough record");
    eprintln!("F6 walkthrough record: {}", path.display());
    for record in records {
        eprintln!(
            "F6 {} runtime_ok={} stop={} tools={:?} changed={:?}",
            record.name, record.runtime_ok, record.stop, record.tools, record.files_changed
        );
        assert!(
            record.runtime_ok,
            "{} product path failed: {:?}",
            record.name, record.failures
        );
    }
}

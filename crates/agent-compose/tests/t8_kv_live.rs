//! T8 live provider experiment (conditional task, real spend). One fixed
//! task through the real composition root and the pinned real provider:
//!
//! 1. COLD TASK — fresh workspace, read a seeded brief, write a summary
//!    artifact with an exact-content oracle.
//! 2. WARM CONTINUATION — a follow-up user message on the same workspace:
//!    the request prefix contains the whole prior conversation, so a
//!    prefix cache can serve it.
//! 3. CROSS-RUN REPEAT — a fresh workspace running the same goal text: the
//!    system+tools prefix matches run 1's first request, the conversation
//!    does not.
//!
//! Every model round is ledgered from the typed `ModelUsed` event (per-field
//! cache buckets, attempts, call lane). The three T8 states are reported in
//! their separated forms — ENDPOINT_ACCEPTED / SERVER_HIT / NET_TASK_COST —
//! and never merged into one "it works" claim. Ignored in ordinary CI; run
//! explicitly with `eval.env` / `OPENAI_API_KEY` and an authorized budget.
//! The evidence report records MODEL / BASE_URL / protocol only, never the
//! key. Token totals are the honest unit here: dollar normalization needs
//! the provider's price table and stays NOT_RUN without one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, ModelSelection, build_context_engine,
    compose, try_model_from_env,
};
use agent_contracts::{
    RuntimeEvent,
    model::{ModelCallRole, UsageIdentity},
};
use agent_core::PolicyApprovalGate;
use agent_runtime::RuntimeHandle;
use agent_storage::FileEventJournal;
use agent_workspace::{Workspace, WorkspaceOutputBroker};
use tokio::time::Instant;
use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

const FULL_TURN: Duration = Duration::from_secs(6 * 60);
const MAX_TOOL_ROUNDS: usize = 12;

const MARKERS: [&str; 3] = [
    "T8-MARKER-ALPHA-7Q2Z",
    "T8-MARKER-BRAVO-3K9X",
    "T8-MARKER-CHARLIE-5R4W",
];
const FOURTH_MARKER: &str = "T8-MARKER-DELTA-8J6V";

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
        Some(repo_root().join("eval.env")),
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
        if !ALLOWED.contains(&key.trim()) {
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
        unsafe { std::env::set_var(key.trim(), value) };
    }
}

#[derive(Clone, Debug)]
struct CostRow {
    round: usize,
    role: ModelCallRole,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cache_miss_input_tokens: Option<u64>,
    attempts: u32,
    retries: u32,
    usage_identity: UsageIdentity,
}

#[derive(Default)]
struct PhaseLedger {
    rows: Vec<CostRow>,
    tools: BTreeSet<String>,
    failures: Vec<String>,
    turn_completed: bool,
    task_completed: bool,
    timed_out: bool,
}

impl PhaseLedger {
    fn totals(&self) -> (u64, u64, u64, u64, u64) {
        let mut input = 0;
        let mut output = 0;
        let mut cached = 0;
        let mut miss = 0;
        let mut attempts = 0;
        for row in &self.rows {
            input += row.input_tokens;
            output += row.output_tokens;
            cached += row.cached_input_tokens;
            miss += row.cache_miss_input_tokens.unwrap_or(0);
            attempts += u64::from(row.attempts);
        }
        (input, output, cached, miss, attempts)
    }
}

fn report_path() -> PathBuf {
    std::env::var("T8_LIVE_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("docs/walkthroughs/2026-09-18-t8-kv-live.md"))
}

async fn product_compose(root: &Path) -> anyhow::Result<agent_compose::ComposedRuntime> {
    let workspace = Workspace::open(root).await?;
    let model = match try_model_from_env()? {
        ModelSelection::Mock(_) => {
            anyhow::bail!("AGENT_DEMO is set; the T8 live experiment needs a real provider")
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
        None,
        &agent_compose::MaintenanceBudget::default(),
        None,
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
        cache_routing: None,
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
        max_tool_rounds: Some(MAX_TOOL_ROUNDS),
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation_journal),
        verification_recipes: Some(recipes),
        project_proof_refresh: has_recipes,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await
}

async fn start_task(handle: &RuntimeHandle, goal: &str) -> anyhow::Result<()> {
    handle.set_focus(goal.to_string()).await?;
    handle.user_message(goal.to_string()).await?;
    Ok(())
}

async fn drain_phase(
    phase: &'static str,
    ledger: &mut PhaseLedger,
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) {
    let deadline = Instant::now() + FULL_TURN;
    let mut round = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            ledger.timed_out = true;
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(envelope)) => match envelope.event {
                RuntimeEvent::ModelStarted { .. } => {
                    round += 1;
                    eprintln!("T8 {phase} model round {round} started");
                }
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    attempts,
                    retries,
                    usage_identity,
                    role,
                    usage,
                } => {
                    ledger.rows.push(CostRow {
                        round,
                        role,
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        cache_miss_input_tokens: usage
                            .as_ref()
                            .and_then(|usage| usage.cache_miss_input_tokens),
                        attempts,
                        retries,
                        usage_identity,
                    });
                }
                RuntimeEvent::ToolStarted { call } => {
                    eprintln!("T8 {phase} tool {}", call.name);
                    ledger.tools.insert(call.name.clone());
                }
                RuntimeEvent::TaskCompleted { .. } => {
                    ledger.task_completed = true;
                    ledger.turn_completed = true;
                }
                RuntimeEvent::TurnCompleted => {
                    ledger.turn_completed = true;
                }
                RuntimeEvent::Failure { class, message, .. } => {
                    eprintln!("T8 {phase} failure {class:?}: {message}");
                    ledger.failures.push(format!("{class:?}: {message}"));
                }
                _ => {}
            },
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => break,
            Err(_) => {
                ledger.timed_out = true;
                break;
            }
        }
        if ledger.turn_completed {
            break;
        }
    }
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, content) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create seeded dir");
        }
        std::fs::write(path, content).expect("write seeded file");
    }
}

fn brief_content() -> String {
    let mut body = String::from(
        "# T8 evidence brief\n\nThe summary must contain exactly these three lines:\n\n",
    );
    for marker in MARKERS {
        body.push_str(marker);
        body.push('\n');
    }
    body
}

fn summary_has_markers_in_order(body: &str, markers: &[&str]) -> bool {
    let mut cursor = 0usize;
    for marker in markers {
        match body[cursor..].find(marker) {
            Some(at) => cursor += at + marker.len(),
            None => return false,
        }
    }
    true
}

fn render_report(
    identity: &str,
    cold: &PhaseLedger,
    warm: &PhaseLedger,
    cross: &PhaseLedger,
    oracle_cold: bool,
    oracle_warm: bool,
    oracle_cross: bool,
) -> String {
    let mut out = String::new();
    out.push_str("# T8 live KV experiment — real provider evidence\n\n");
    out.push_str(&format!("- serving: `{identity}` (no secrets recorded)\n"));
    out.push_str("- task: read seeded brief → write summary artifact; byte-level marker oracle\n");
    out.push_str("- phases: cold task / warm continuation (same workspace) / cross-run repeat (fresh workspace)\n\n");
    for (name, ledger, oracle) in [
        ("cold task", cold, oracle_cold),
        ("warm continuation", warm, oracle_warm),
        ("cross-run repeat", cross, oracle_cross),
    ] {
        let (input, output, cached, miss, attempts) = ledger.totals();
        out.push_str(&format!(
            "| {name} | rounds={} | main lanes={} | input={} | output={} | cache_hit={} | cache_miss={} | attempts={} | oracle={} |\n",
            ledger.rows.len(),
            ledger
                .rows
                .iter()
                .filter(|row| row.role == ModelCallRole::Main)
                .count(),
            input,
            output,
            cached,
            miss,
            attempts,
            if oracle { "met" } else { "MISSED" },
        ));
        for row in &ledger.rows {
            out.push_str(&format!(
                "  - round={} role={:?} in={} out={} hit={} miss={:?} attempts={} retries={} identity={:?}\n",
                row.round,
                row.role,
                row.input_tokens,
                row.output_tokens,
                row.cached_input_tokens,
                row.cache_miss_input_tokens,
                row.attempts,
                row.retries,
                row.usage_identity,
            ));
        }
    }
    let (_, _, cold_hit, _, _) = cold.totals();
    let (_, _, warm_hit, _, _) = warm.totals();
    let (_, _, cross_hit, _, _) = cross.totals();
    out.push_str("\n## Separated conclusions\n\n");
    let accepted = cold.turn_completed
        && warm.turn_completed
        && cross.turn_completed
        && cold.failures.is_empty()
        && warm.failures.is_empty()
        && cross.failures.is_empty()
        && oracle_cold
        && oracle_warm
        && oracle_cross;
    out.push_str(&format!(
        "- ENDPOINT_ACCEPTED: {}\n",
        if accepted {
            "PASS — every round of every phase completed through the pinned protocol, no runtime failures, all artifact oracles met"
        } else {
            "FAIL — see per-phase completion/failure/oracle rows above"
        }
    ));
    if warm_hit > 0 || cold_hit > 0 {
        out.push_str(&format!(
            "- SERVER_HIT: OBSERVED — provider-reported prefix-cache hits within the production trajectory (warm continuation hit={warm_hit}, cold task hit={cold_hit}); cross-run first-request hit={cross_hit} (auto-cache is provider-side; cross-run reuse is not guaranteed)\n"
        ));
    } else {
        out.push_str(
            "- SERVER_HIT: NOT_OBSERVED — the provider reported no prefix-cache hits for any round of this trajectory (auto-cache is provider-side and may need larger/slower repeated prefixes)\n",
        );
    }
    let (in_cold, out_cold, hit_cold, miss_cold, att_cold) = cold.totals();
    let (in_warm, out_warm, hit_warm, miss_warm, att_warm) = warm.totals();
    let (in_cross, out_cross, _, _, att_cross) = cross.totals();
    out.push_str(
        "- NET_TASK_COST (token accounting; dollar normalization NOT_RUN — needs the provider price table):\n",
    );
    out.push_str(&format!(
        "  - cold task: input={in_cold} (hit={hit_cold}, miss={miss_cold}) output={out_cold} attempts={att_cold}\n"
    ));
    out.push_str(&format!(
        "  - warm continuation: input={in_warm} (hit={hit_warm}) output={out_warm} attempts={att_warm}\n"
    ));
    out.push_str(&format!(
        "  - cross-run repeat: input={in_cross} output={out_cross} attempts={att_cross}\n"
    ));
    if hit_warm > 0 {
        let discount = hit_warm as f64 / (hit_warm as f64 + miss_warm as f64).max(1.0);
        out.push_str(&format!(
            "  - warm-continuation prefix reuse: {:.1}% of reported input served from the provider cache (hit/(hit+miss); provider prices determine the actual discount)\n",
            discount * 100.0
        ));
    }
    out.push_str("\n- honesty: one trajectory, one serving, one run — a walkthrough record and cost observation, not a benchmark; scripted-provider ledgers elsewhere prove transport/settlement, this file adds the real endpoint's own reported usage.\n");
    out
}

#[ignore = "T8 live provider experiment: real spend; needs eval.env / OPENAI_API_KEY and an authorized budget"]
#[tokio::test(flavor = "multi_thread")]
async fn t8_live_endpoint_acceptance_prefix_hits_and_task_cost() {
    load_eval_env();
    let identity = format!(
        "{} @ {} protocol={}",
        std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "<unset>".into()),
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "<unset>".into()),
        std::env::var("OPENAI_API_PROTOCOL").unwrap_or_else(|_| "chat".into()),
    );

    let goal = "Read brief/t8_brief.md and write out/t8_summary.md whose entire content is exactly the three marker lines from the brief, one per line, in the same order. Do not add any other text.";
    let follow_up = format!(
        "Append a fourth line to out/t8_summary.md so the file ends with exactly this line: {FOURTH_MARKER}. Keep the existing three lines unchanged."
    );

    // Phase 1 + 2: one workspace, cold task then warm continuation.
    let workspace_a = tempfile::tempdir().expect("temp workspace");
    write_files(
        workspace_a.path(),
        &[("brief/t8_brief.md", &brief_content())],
    );
    let composed_a = product_compose(workspace_a.path())
        .await
        .expect("compose cold workspace");
    composed_a.instance.start().await.expect("start runtime A");
    let mut events_a = composed_a.subscribe();
    start_task(composed_a.handle(), goal)
        .await
        .expect("start cold task");
    let mut cold = PhaseLedger::default();
    drain_phase("cold", &mut cold, &mut events_a).await;
    let summary_path = workspace_a.path().join("out/t8_summary.md");
    let oracle_cold = std::fs::read_to_string(&summary_path)
        .map(|body| summary_has_markers_in_order(&body, &MARKERS))
        .unwrap_or(false);

    // Warm continuation on the same workspace: the follow-up request
    // carries the whole prior conversation as its prefix.
    composed_a
        .handle()
        .user_message(follow_up.clone())
        .await
        .expect("queue warm follow-up");
    let mut warm = PhaseLedger::default();
    drain_phase("warm", &mut warm, &mut events_a).await;
    let warm_expect: Vec<&str> = MARKERS.iter().copied().chain([FOURTH_MARKER]).collect();
    let oracle_warm = std::fs::read_to_string(&summary_path)
        .map(|body| summary_has_markers_in_order(&body, &warm_expect))
        .unwrap_or(false);
    composed_a.shutdown().await.expect("shutdown workspace A");

    // Phase 3: fresh workspace, same goal text — only the system+tools
    // prefix can match the first run; the conversation cannot.
    let workspace_b = tempfile::tempdir().expect("temp workspace B");
    write_files(
        workspace_b.path(),
        &[("brief/t8_brief.md", &brief_content())],
    );
    let composed_b = product_compose(workspace_b.path())
        .await
        .expect("compose cross-run workspace");
    composed_b.instance.start().await.expect("start runtime B");
    let mut events_b = composed_b.subscribe();
    start_task(composed_b.handle(), goal)
        .await
        .expect("start cross-run task");
    let mut cross = PhaseLedger::default();
    drain_phase("cross", &mut cross, &mut events_b).await;
    let oracle_cross = std::fs::read_to_string(workspace_b.path().join("out/t8_summary.md"))
        .map(|body| summary_has_markers_in_order(&body, &MARKERS))
        .unwrap_or(false);
    composed_b.shutdown().await.expect("shutdown workspace B");

    // Evidence first, assertions second: a failing phase must still leave
    // the full ledger report on disk for the receipt.
    let report = render_report(
        &identity,
        &cold,
        &warm,
        &cross,
        oracle_cold,
        oracle_warm,
        oracle_cross,
    );
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create report dir");
    }
    std::fs::write(&path, &report).expect("write T8 evidence report");
    println!(
        "\n==== T8 LIVE REPORT ({}) ====\n{}",
        path.display(),
        report
    );

    // Typed-fact sanity: every row in a live run must be provider-observed
    // (Unknown exists for cancelled/legacy rows, which a clean live run has
    // none of), and every round must have at least one attempt.
    for row in cold.rows.iter().chain(&warm.rows).chain(&cross.rows) {
        assert_eq!(
            row.usage_identity,
            UsageIdentity::Observed,
            "live ledger row with non-observed usage: {row:?}"
        );
        assert!(
            row.attempts >= 1,
            "live ledger row with no attempts: {row:?}"
        );
    }
    assert!(
        !cold.rows.is_empty() && !warm.rows.is_empty() && !cross.rows.is_empty(),
        "a phase produced no ModelUsed rows"
    );
    assert!(
        cold.turn_completed && cold.failures.is_empty() && oracle_cold,
        "cold task did not complete cleanly: {:?}",
        cold.failures
    );
    assert!(
        warm.turn_completed && oracle_warm,
        "warm continuation did not complete cleanly: {:?}",
        warm.failures
    );
    assert!(
        cross.turn_completed && cross.failures.is_empty() && oracle_cross,
        "cross-run task did not complete cleanly: {:?}",
        cross.failures
    );
}

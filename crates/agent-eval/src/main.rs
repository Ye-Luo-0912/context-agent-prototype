//! Headless live evaluation: run a context-retention workload through the
//! A/B/C context engines with a real model (OpenAI-compatible, configured
//! via `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OPENAI_MODEL`) and compare
//! token cost against task success.
//!
//! The default task uses no tools so the comparison measures context
//! retention directly: five constraints up front, noise turns, then a
//! question only answerable from what the context frame retained. The
//! endpoint does accept function calling; dotted Core ids are mapped by
//! `provider-openai`. Coding live runs use `--fixture-live`.
// metrics/bundle 的大 json! 字面量需要更深的宏展开。
#![recursion_limit = "256"]

mod acceptance;
mod analysis;
mod bundle;
mod context_bench;
mod context_mech;
mod convergence_bench;
mod driver;
mod envfile;
mod fixture_driver;
mod harvest;
mod hygiene;
mod long_live;
mod long_task;
mod longflow;
mod metrics;
mod mock_model;
mod pilot;
mod retrieval;
mod suite;
mod task;
mod tool_edit_gate;
mod tool_edit_pack;
mod workload;

fn usage() -> ! {
    eprintln!(
        "usage: agent-eval [--engine append|rolling|dynamic] [--all]\n\
         \n\
         Runs the constraint-retention task through the selected context\n\
         engine(s) with a real model and prints token cost vs. task success.\n\
         Requires OPENAI_API_KEY (eval.env or the process environment;\n\
         optionally OPENAI_BASE_URL / OPENAI_MODEL). Never put the key in git.\n\
         \n\
         usage: agent-eval --fixtures\n\
         \n\
         Lists the M15 evaluation inputs — the A/B/C/D tool-surface arms and\n\
         the coding workload fixtures (seed + hidden verification) — without\n\
         calling a model.\n\
         \n\
         usage: agent-eval --metrics <trace.jsonl>\n\
         \n\
         Aggregates the all-module cost accounting of one run's event trace\n\
         (model/schema tokens, GC and lifecycle cost, tool behavior) without\n\
         calling a model.\n\
         \n\
         usage: agent-eval --fixture <id>\n\
         \n\
         Runs one coding fixture end to end against the real builtin tool\n\
         surface with a scripted model (no provider), scores it with the\n\
         hidden verification and prints the cost accounting. Deterministic\n\
         harness smoke test for M15.\n\
         \n\
         usage: agent-eval --fixture-live <id>\n\
         \n\
         Same harness with a real model (eval.env or OPENAI_API_KEY /\n\
         OPENAI_BASE_URL / OPENAI_MODEL); the provider must accept tool calls.\n\
         \n\
         usage: agent-eval --compare-arm <id>\n\
         \n\
         Runs one coding fixture through the append-only, rolling-summary\n\
         and dynamic engines on the same multi-turn scripted model and the\n\
         same real tool surface, printing a cost comparison. The dynamic\n\
         engine must pass the fixture while feeding the model fewer input\n\
         tokens — the deterministic CI proxy of the M15 acceptance.\n\
         \n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live <id>
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live-all
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live-reasonable
         (all live evidence runs refuse a dirty workspace unless --allow-dirty;
         the manifest records source_tree_digest covering HEAD tree + tracked
         diff + untracked crates sources either way)\n\
         \n\
         Same three engines and hidden verification as --compare-arm, but\n\
         each cell uses a real model (OPENAI_*) on a fresh workspace.\n\
         Each fixture sends its live_turns (one prompt for the original\n\
         four). Tool-loop capped. Put --repeats first (1..=8).\n\
         --compare-live-reasonable runs only add_test.\n\
         --compare-live-all excludes recall_after_fix.\n\
         --compare-live / --fixture-live recall_after_fix is refused:\n\
         that diagnostic is complete (scripted --compare-arm remains).\n\
         Next mechanism live is --context-mech-run. Live coding\n\
         cells use production ToolLifecycleConfig::default() (write/edit/\n\
         context.manage stay catalog-only except NeedEvidence). Scripted\n\
         --compare-arm, fixtures, and context-bench/mech ops still pin\n\
         write/edit and context.manage. Live cells always write a versioned\n\
         evidence bundle (default target/eval-evidence/<unix-secs>). This\n\
         is the live paired smoke, not the 300×3 gate.\n\
         \n\
         usage: agent-eval [--repeats N] --long-task-live [normal|resume]\n\
         \n\
         usage: agent-eval [--repeats N] --opportunity-gate [normal|resume]\n\
         \n\
         Item-8 off/on paired live gate for the advisory completion\n\
         opportunity: identical cells with the candidate switch as the only\n\
         variable; evidence records the setting per cell.\n\
         \n\
         retry_policy_dev live pilot (layer 2): the C engine runs the frozen\n\
         one-directive fixture with a real model. Resume cells interrupt on\n\
         the semantic trigger (first durably settled mutation + its durable\n\
         checkpoint), then stop/restore/continue the same directive.\n\
         Acceptance: TaskCompleted + hidden cargo tests pass + allowed diff.\n\
         Writes evidence under crates/agent-eval/evidence/retry-pilot/.\n\
         \n\
         usage: agent-eval --long-task-gate\n\
         \n\
         usage: agent-eval --show-evidence <cell-dir|pair-dir>\n\
         \n\
         Rebuild the comparison/tool table from a persisted evidence bundle.\n\
         \n\
         usage: agent-eval --suite\n\
         \n\
         Print the acceptance-suite pack status (n/300, freeze blockers).\n\
         Smoke FIXTURES do not count. File-harvested tasks plus SWE-bench\n\
         Verified docker instances. Pack freeze is not the analysis gate\n\
         Pack freeze is not the 300×3 acceptance run.\n\
         \n\
         usage: agent-eval --suite-check\n\
         \n\
         Self-check the 9 file-harvested tasks (seed fails, expected passes).\n\
         Does not pull SWE-bench images.\n\
         \n\
         usage: agent-eval --swebench-gold [instance_id]\n\
         \n\
         Opt-in official harness gold eval for one Verified instance\n\
         (default pallets__flask-5014). Requires Docker. Set\n\
         AGENT_EVAL_SWEBENCH_DOCKER=1. Does not pull all 500 images.\n\
         \n\
         usage: agent-eval --preregister\n\
         \n\
         Print the frozen EVAL-01.3c analysis spec, spec hash, and power\n\
         simulation (historical 30×3 plus the 300×3 design). The suite pack\n\
         is frozen; the gate requires the exact 300 acceptance ids, not any\n\
         ≥300 subset of the 509 pack. 300×3 acceptance cells wait on the\n\
         remaining calibration. This does not close M15.\n\
         \n\
         usage: agent-eval --acceptance\n\
         \n\
         Print the frozen exact 300 acceptance ids and sha256.\n\
         \n\
         usage: agent-eval --pilot\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] [--include-swebench] [--pilot-id <id>] --pilot-run\n\
         usage: agent-eval [--file-only] --pilot-calibrate <dir>\n\
         \n\
         Frozen EVAL-01.5 sample (30 ids, 10/10/10 size). --pilot lists it.\n\
         --pilot-run is live A/B/C on that sample; default file-only (9 tasks).\n\
         --include-swebench clones GitHub at base_commit and scores a git\n\
         diff with the official Docker harness (AGENT_EVAL_SWEBENCH_CLONE=1\n\
         and AGENT_EVAL_SWEBENCH_DOCKER=1). Calibration design is 3 repeats;\n\
         --pilot-calibrate prints decision=pilot and never opens the gate.\n\
         --file-only keeps pack file-runtime tasks so P0 SWE-bench floor\n\
         cells cannot mix into the 9-task ITT table. Do not collect 300×3\n\
         acceptance cells from this command.\n\
         \n\
         usage: agent-eval --analyze-evidence <dir>\n\
         \n\
         Rebuild the predeclared C-A clustered interval from EVAL-01.1\n\
         bundles. Eligible only when the suite is frozen and the cell set\n\
         meets 300×3. The ~30×3 calibration pilot is not acceptance.\n\
         \n\
         usage: agent-eval --retrieval\n\
         \n\
         Engine-only retrieval baseline: GC-externalize unique facts, then\n\
         measure search recall/latency and graded access stamps. Not the\n\
         paired real-model coding gate.\n\
         \n\
         usage: agent-eval --retrieval-complex\n\
         \n\
         Engine-only COMPLEX retrieval scenario through the real call chain\n\
         (ingest → GC externalize → search_external): multi-word semantic\n\
         needles that are never contiguous in any summary, one kind-filtered\n\
         case, single-word and identity controls as regression guards.\n\
         Prints per-case RC rows. Not the paired real-model coding gate.\n\
         \n\
         usage: agent-eval --context-hygiene\n\
         \n\
         Engine-only C-hygiene ablation: current / descriptor-only /\n\
         one-file-body on a scripted trajectory. Measures old-tool-body\n\
         auto-reactivation (P3) and fs.read reread classes (P4). No\n\
         provider. Does not rewrite SPEC and does not enable those\n\
         switches in production C.\n\
         \n\
         usage: agent-eval --context-bench\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --context-bench-run [id]\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --context-bench-ablation\n\
         \n\
         EVAL-02 Context Benchmark: 12 tasks that ask where dynamic context\n\
         helps or hurts a coding agent. --context-bench prints the pack.\n\
         --context-bench-run is live A/C (rolling only on horizon_long,\n\
         semantic_recall, task_switch). Wave 1 is 24+3 cells at repeats=1.\n\
         --context-bench-ablation is C-only on semantic_recall: current /\n\
         force-compact / no-progress, default repeats=2, shuffled arm order.\n\
         It does not rewrite SPEC or the frozen pack. Default evidence dir\n\
         is crates/agent-eval/evidence/context-bench-ablation/.\n\
         Do not keep live-running semantic_recall.v1 after CI is green.\n\
         \n\
         usage: agent-eval --context-mech\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --context-mech-run [id]\n\
         \n\
         Mechanism V2: three one-mechanism scenarios. late_semantic_constraint\n\
         is the non-Anchor GC-recall test; resume_operational_state covers\n\
         verify→mutate→switch→resume freshness; no_semantic_episode is the\n\
         distill-skip case. Live is A/C (no rolling): 3 tasks × 2 engines\n\
         × 2 repeats = 12 cells. Default repeats=2. Does not rewrite frozen\n\
         context-bench.v1. Do not keep live-running recall_after_fix. This\n\
         does not close M15 and does not open the 300×3 ITT gate.\n\
         \n\
         usage: agent-eval --tool-edit\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --tool-edit-run [id]\n\
         \n\
         Tool Edit V2: four raw-byte fixtures on one fixed dynamic engine\n\
         and the production-default tool surface. The live default is three\n\
         repeats (12 cells). It measures edit.patch, not Context policy,\n\
         and writes one versioned evidence bundle per cell.\n\
         \n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --longflow-run [id]\n\
         \n\
         Long-flow diagnostic: the 15-turn late-constraint trajectory on\n\
         append vs dynamic, run CONCURRENTLY so wall time stays close to\n\
         one cell. Development instrument only; evidence dir default is\n\
         crates/agent-eval/evidence/longflow/.\n"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Some(path) = envfile::load()? {
        eprintln!("{}", envfile::status_line(&path));
    }
    let mut engines: Vec<&'static str> = Vec::new();
    let mut repeats: u32 = 1;
    let mut repeats_set = false;
    let mut evidence_dir: Option<std::path::PathBuf> = None;
    let mut include_swebench = false;
    let mut file_only = false;
    let mut pilot_id: Option<String> = None;
    // EVAL-03 identity gate: live evidence runs refuse a dirty tree unless
    // the operator explicitly accepts a source_tree_digest diagnostic.
    let mut allow_dirty = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => {
                workload::verify_fixture_inputs()?;
                print!("{}", workload::render_fixtures());
                return Ok(());
            }
            "--suite" => {
                print!("{}", suite::render_suite(&suite::load_pack()?));
                return Ok(());
            }
            "--suite-check" => {
                println!("{}", suite::check_file_harvest(&suite::load_pack()?)?);
                return Ok(());
            }
            "--swebench-gold" => {
                if !harvest::docker_opt_in() {
                    anyhow::bail!(
                        "set AGENT_EVAL_SWEBENCH_DOCKER=1 to run official harness gold eval"
                    );
                }
                let instance_id = args
                    .next()
                    .filter(|value| !value.starts_with('-'))
                    .unwrap_or_else(|| harvest::GOLD_SMOKE_INSTANCE.to_string());
                let instance_id = harvest::instance_id_from_suite_id(&instance_id)
                    .unwrap_or(instance_id.as_str())
                    .to_string();
                let result = harvest::run_gold_eval(&instance_id)?;
                println!(
                    "swebench-gold {instance_id} passed={} exit={:?} timed_out={}",
                    result.passed, result.exit, result.timed_out
                );
                if !result.stdout.is_empty() {
                    println!("{}", result.stdout);
                }
                if !result.stderr.is_empty() {
                    eprintln!("{}", result.stderr);
                }
                if !result.passed {
                    anyhow::bail!("swebench gold eval did not resolve {instance_id}");
                }
                return Ok(());
            }
            "--fixture" => {
                let Some(id) = args.next() else {
                    usage();
                };
                let fixture = workload::FIXTURES
                    .iter()
                    .find(|fixture| fixture.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown fixture: {id} (see --fixtures)"))?;
                let dir = tempfile::tempdir()?;
                workload::seed_fixture(fixture, dir.path());
                let eval = fixture_driver::run_fixture(fixture, dir.path()).await?;
                print!(
                    "fixture {}: passed={}\n{}",
                    eval.fixture_id,
                    eval.passed,
                    metrics::render_metrics(&eval.metrics)
                );
                return Ok(());
            }
            "--fixture-live" => {
                let Some(id) = args.next() else {
                    usage();
                };
                if let Some(reason) = workload::live_coding_refused(&id) {
                    anyhow::bail!("{reason}");
                }
                let fixture = workload::FIXTURES
                    .iter()
                    .find(|fixture| fixture.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown fixture: {id} (see --fixtures)"))?;
                let model = driver::build_live_coding_model()?;
                let dir = tempfile::tempdir()?;
                workload::seed_fixture(fixture, dir.path());
                let eval =
                    fixture_driver::run_fixture_with_model(fixture, dir.path(), model).await?;
                print!(
                    "fixture {} (live): passed={} wall_ms={}\n{}",
                    eval.fixture_id,
                    eval.passed,
                    eval.wall_ms,
                    metrics::render_metrics(&eval.metrics)
                );
                return Ok(());
            }
            "--repeats" => {
                let Some(value) = args.next() else {
                    usage();
                };
                repeats = value.parse().map_err(|_| {
                    anyhow::anyhow!("--repeats needs an integer 1..=8, got {value}")
                })?;
                if !(1..=8).contains(&repeats) {
                    anyhow::bail!("--repeats must be 1..=8 (harness smoke, not the 300×3 gate)");
                }
                repeats_set = true;
            }
            "--evidence-dir" => {
                let Some(value) = args.next() else {
                    usage();
                };
                evidence_dir = Some(std::path::PathBuf::from(value));
            }
            "--show-evidence" => {
                let Some(path) = args.next() else {
                    usage();
                };
                print!("{}", bundle::render_evidence(std::path::Path::new(&path))?);
                return Ok(());
            }
            "--preregister" => {
                print!("{}", analysis::render_preregister());
                return Ok(());
            }
            "--acceptance" => {
                print!("{}", acceptance::render_acceptance(&suite::load_pack()?)?);
                return Ok(());
            }
            "--include-swebench" => {
                include_swebench = true;
            }
            "--file-only" => {
                file_only = true;
            }
            "--allow-dirty" => {
                allow_dirty = true;
            }
            "--pilot-id" => {
                let Some(id) = args.next() else {
                    usage();
                };
                pilot_id = Some(id);
            }
            "--pilot" => {
                let sample = pilot::select_pilot(&suite::load_pack()?)?;
                print!("{}", pilot::render_pilot(&sample));
                return Ok(());
            }
            "--pilot-run" => {
                run_pilot_live(
                    pilot_id,
                    repeats,
                    evidence_dir,
                    include_swebench,
                    allow_dirty,
                )
                .await?;
                return Ok(());
            }
            "--pilot-calibrate" => {
                let Some(path) = args.next() else {
                    usage();
                };
                let report = pilot::load_and_calibrate(std::path::Path::new(&path), file_only)?;
                print!("{}", pilot::render_calibration(&report));
                return Ok(());
            }
            "--analyze-evidence" => {
                let Some(path) = args.next() else {
                    usage();
                };
                let cells = analysis::load_evidence_root(std::path::Path::new(&path))?;
                print!("{}", analysis::render_report(&analysis::analyze(&cells)));
                return Ok(());
            }
            "--compare-live" => {
                let Some(id) = args.next() else {
                    usage();
                };
                if let Some(reason) = workload::live_coding_refused(&id) {
                    anyhow::bail!("{reason}");
                }
                let fixture = workload::FIXTURES
                    .iter()
                    .find(|fixture| fixture.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown fixture: {id} (see --fixtures)"))?;
                run_live_compare(&[fixture], repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--compare-live-all" => {
                let fixtures: Vec<&workload::CodingFixture> =
                    workload::live_coding_compare_fixtures().collect();
                run_live_compare(&fixtures, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--compare-live-reasonable" => {
                let fixtures: Vec<&workload::CodingFixture> = workload::FIXTURES
                    .iter()
                    .filter(|fixture| fixture.id == "add_test")
                    .collect();
                run_live_compare(&fixtures, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--compare-arm" => {
                let Some(id) = args.next() else {
                    usage();
                };
                let fixture = workload::FIXTURES
                    .iter()
                    .find(|fixture| fixture.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown fixture: {id} (see --fixtures)"))?;
                let dir = tempfile::tempdir()?;
                workload::seed_fixture(fixture, dir.path());
                let runs = fixture_driver::compare_engines(fixture, dir.path()).await?;
                print!("{}", fixture_driver::render_comparison(&runs));
                return Ok(());
            }
            "--metrics" => {
                let Some(path) = args.next() else {
                    usage();
                };
                let events = read_trace(&path)?;
                let metrics = metrics::aggregate_metrics(&events);
                print!("{}", metrics::render_metrics(&metrics));
                return Ok(());
            }
            "--retrieval" => {
                let report = retrieval::run_retrieval_baseline().await?;
                print!("{}", retrieval::render_retrieval(&report));
                return Ok(());
            }
            "--retrieval-complex" => {
                let report = retrieval::run_retrieval_complex_baseline().await?;
                print!("{report}");
                return Ok(());
            }
            "--context-hygiene" => {
                let reports = hygiene::run_hygiene_ablation().await?;
                print!("{}", hygiene::render_hygiene(&reports));
                return Ok(());
            }
            "--tool-edit" => {
                let pack = tool_edit_pack::load_pack()?;
                print!("{}", tool_edit_pack::render_pack(&pack));
                print!("{}", tool_edit_pack::check_pack(&pack)?);
                return Ok(());
            }
            "--tool-edit-run" => {
                let only = args.next().filter(|value| !value.starts_with('-'));
                let repeats = if repeats_set {
                    repeats
                } else {
                    tool_edit_pack::DEFAULT_REPEATS
                };
                run_tool_edit_live(only, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--context-bench" => {
                let pack = context_bench::load_pack()?;
                print!("{}", context_bench::render_pack(&pack));
                print!("{}", context_bench::check_pack(&pack)?);
                return Ok(());
            }
            "--context-bench-run" => {
                let only = args.next().filter(|value| !value.starts_with('-'));
                run_context_bench_live(only, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--context-bench-ablation" => {
                let repeats = if repeats_set { repeats } else { 2 };
                run_context_bench_ablation(repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--context-mech" => {
                let pack = context_mech::load_pack()?;
                print!("{}", context_mech::render_pack(&pack));
                print!("{}", context_mech::check_pack(&pack)?);
                return Ok(());
            }
            "--context-mech-run" => {
                let only = args.next().filter(|value| !value.starts_with('-'));
                let repeats = if repeats_set { repeats } else { 2 };
                run_context_mech_live(only, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--longflow-run" => {
                let only = args.next().filter(|value| !value.starts_with('-'));
                let repeats = if repeats_set { repeats } else { 1 };
                run_longflow_live(only, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--convergence-bench" => {
                let reports = convergence_bench::run_convergence_bench().await?;
                for report in &reports {
                    println!(
                        "{:22} {} {}",
                        report.scenario,
                        if report.passed { "PASS" } else { "FAIL" },
                        report.detail
                    );
                }
                if !reports.iter().all(|report| report.passed) {
                    anyhow::bail!("convergence bench failed");
                }
                return Ok(());
            }
            "--long-task-gate" => {
                let report = long_task::run_deterministic_gate().await?;
                println!(
                    "retry_policy_dev deterministic gate: {}",
                    if report.passed() { "PASS" } else { "FAIL" }
                );
                println!(
                    "resume_committed={} checkpoint_durable={} continuation={} completed={} order_ok={}",
                    report.resume_committed,
                    report.checkpoint_durable,
                    report.continuation_started,
                    report.task_completed,
                    report.order_ok
                );
                if let Some(duplicate) = &report.duplicated_effect {
                    println!("duplicated_effect: {duplicate}");
                }
                for violation in &report.hidden_violations {
                    println!("hidden: {violation}");
                }
                if !report.passed() {
                    anyhow::bail!("long-task gate failed");
                }
                let opportunity = long_task::run_opportunity_replay().await?;
                println!(
                    "completion_opportunity off/on replay: {}",
                    if opportunity.passed() { "PASS" } else { "FAIL" }
                );
                if !opportunity.passed() {
                    anyhow::bail!("completion-opportunity replay failed");
                }
                return Ok(());
            }
            "--opportunity-gate" => {
                let mode_filter = args.next().filter(|value| !value.starts_with('-'));
                let repeats = if repeats_set {
                    repeats
                } else {
                    long_live::DEFAULT_REPEATS
                };
                run_opportunity_gate(mode_filter, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--long-task-live" => {
                let mode_filter = args.next().filter(|value| !value.starts_with('-'));
                let repeats = if repeats_set {
                    repeats
                } else {
                    long_live::DEFAULT_REPEATS
                };
                run_long_task_live(mode_filter, repeats, evidence_dir, allow_dirty).await?;
                return Ok(());
            }
            "--all" => engines = vec!["append", "rolling", "dynamic"],
            "--engine" => {
                let Some(value) = args.next() else {
                    usage();
                };
                let engine: &'static str = match value.as_str() {
                    "append" => "append",
                    "rolling" => "rolling",
                    "dynamic" => "dynamic",
                    other => {
                        eprintln!("unknown engine: {other}");
                        usage();
                    }
                };
                engines.push(engine);
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    if engines.is_empty() {
        engines = vec!["dynamic"];
    }

    let prompts = task::prompts();
    let mut results = Vec::new();
    for engine in &engines {
        eprintln!("== running engine: {engine} ==");
        let summary = driver::run_eval(engine, engine, &prompts, task::verify).await?;
        results.push(summary);
    }

    print!("{}", driver::render_comparison(&results));
    Ok(())
}

/// Read a JSONL event trace and keep the first run's envelopes.
fn read_trace(path: &str) -> anyhow::Result<Vec<agent_contracts::RuntimeEventEnvelope>> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("read trace {path}: {error}"))?;
    let mut envelopes = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let envelope: agent_contracts::RuntimeEventEnvelope = serde_json::from_str(line)
            .map_err(|error| anyhow::anyhow!("parse trace line {}: {error}", index + 1))?;
        envelopes.push(envelope);
    }
    let Some(first) = envelopes.first() else {
        anyhow::bail!("trace {path} contains no events");
    };
    let run_id = first.run_id;
    envelopes.retain(|envelope| envelope.run_id == run_id);
    Ok(envelopes)
}

async fn run_live_compare(
    fixtures: &[&workload::CodingFixture],
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir.unwrap_or_else(default_evidence_dir);
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    for fixture in fixtures {
        for round in 1..=repeats {
            eprintln!(
                "== live compare {} engine-pair repeat {round}/{repeats} order={:?} ==",
                fixture.id,
                analysis::arm_order(fixture.id, round)
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                fixture.id.to_string(),
                round,
                repeats,
                true,
            );
            let runs = fixture_driver::compare_engines_live(
                fixture,
                dir.path(),
                model.clone(),
                Some(&pair),
            )
            .await?;
            if repeats > 1 {
                println!("fixture {} repeat {round}/{repeats}", fixture.id);
            }
            print!("{}", fixture_driver::render_live_comparison(&runs));
            print!("{}", bundle::render_evidence(&pair.repeat_path())?);
        }
    }
    Ok(())
}

async fn run_pilot_live(
    only_id: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    include_swebench: bool,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = suite::load_pack()?;
    let sample = pilot::select_pilot(&pack)?;
    let mut tasks: Vec<&suite::SuiteTask> = sample.tasks.iter().collect();
    if let Some(id) = &only_id {
        tasks.retain(|task| task.id == *id);
        if tasks.is_empty() {
            anyhow::bail!("{id} is not in the frozen 30-task calibration sample (see --pilot)");
        }
    }
    if include_swebench {
        if !harvest::clone_opt_in() || !harvest::docker_opt_in() {
            anyhow::bail!(
                "--include-swebench requires AGENT_EVAL_SWEBENCH_CLONE=1 and AGENT_EVAL_SWEBENCH_DOCKER=1"
            );
        }
    } else {
        tasks.retain(|task| pilot::is_file_runtime(task));
    }
    if tasks.is_empty() {
        anyhow::bail!("no pilot tasks selected");
    }
    let cells = (tasks.len() as u32)
        .saturating_mul(repeats)
        .saturating_mul(3);
    eprintln!(
        "pilot-run n={} repeats={} engines=3 cells={} include_swebench={} sample={}",
        tasks.len(),
        repeats,
        cells,
        include_swebench,
        sample.sha256
    );
    if repeats != analysis::GATE_REPEATS {
        eprintln!(
            "warning: calibration design is {} repeats; this run uses {repeats}",
            analysis::GATE_REPEATS
        );
    }
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir.unwrap_or_else(default_evidence_dir);
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    for task in tasks {
        for round in 1..=repeats {
            let pair_dir = evidence_root.join(&task.id).join(format!("r{round}"));
            if pair_dir.join("pair.json").is_file() {
                // 已有 pair 不重跑，才能从 file-only 81 格 resume 到剩余 SWE-bench。
                eprintln!(
                    "== skip existing {} repeat {round}/{repeats} ({}) ==",
                    task.id,
                    pair_dir.display()
                );
                continue;
            }
            eprintln!(
                "== pilot {} engine-pair repeat {round}/{repeats} order={:?} ==",
                task.id,
                analysis::arm_order(&task.id, round)
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                task.id.clone(),
                round,
                repeats,
                true,
            );
            let runs =
                fixture_driver::compare_suite_live(task, dir.path(), model.clone(), Some(&pair))
                    .await?;
            println!("task {} repeat {round}/{repeats}", task.id);
            print!("{}", fixture_driver::render_live_comparison(&runs));
            print!("{}", bundle::render_evidence(&pair.repeat_path())?);
        }
    }
    eprintln!(
        "pilot cells written under {}. --pilot-calibrate that directory; decision=pilot.",
        evidence_root.display()
    );
    Ok(())
}

async fn run_context_bench_live(
    only_id: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = context_bench::load_pack()?;
    eprintln!("{}", context_bench::check_pack(&pack)?);
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir.unwrap_or_else(default_evidence_dir);
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    let tasks: Vec<&context_bench::BenchTask> = pack
        .tasks
        .iter()
        .filter(|task| only_id.as_ref().is_none_or(|id| task.id() == id))
        .collect();
    if tasks.is_empty() {
        anyhow::bail!(
            "no context-bench task matches {:?} (see --context-bench)",
            only_id
        );
    }
    for task in tasks {
        for round in 1..=repeats {
            eprintln!(
                "== context-bench {} engines={:?} repeat {round}/{repeats} ==",
                task.id(),
                fixture_driver::bench_arm_order(task, round)
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                task.id().to_string(),
                round,
                repeats,
                true,
            );
            let runs = fixture_driver::compare_bench_live(
                &pack,
                task,
                dir.path(),
                model.clone(),
                Some(&pair),
            )
            .await?;
            print!("{}", fixture_driver::render_live_comparison(&runs));
            let pair_dir = pair.repeat_path();
            print!("{}", bundle::render_evidence(&pair_dir)?);
        }
    }
    Ok(())
}

async fn run_context_bench_ablation(
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = context_bench::load_pack()?;
    eprintln!("{}", context_bench::check_pack(&pack)?);
    let task = pack
        .tasks
        .iter()
        .find(|task| task.id() == "semantic_recall")
        .ok_or_else(|| anyhow::anyhow!("context-bench pack is missing semantic_recall"))?;
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir.unwrap_or_else(|| {
        std::path::PathBuf::from("crates/agent-eval/evidence/context-bench-ablation")
    });
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    for round in 1..=repeats {
        eprintln!(
            "== context-bench-ablation semantic_recall arms={:?} repeat {round}/{repeats} ==",
            fixture_driver::ablation_arm_order(round)
        );
        let dir = tempfile::tempdir()?;
        let pair = bundle::PairSink::claim(
            evidence_root.clone(),
            task.id().to_string(),
            round,
            repeats,
            true,
        );
        let runs = fixture_driver::compare_ablation_live(
            &pack,
            task,
            dir.path(),
            model.clone(),
            Some(&pair),
        )
        .await?;
        print!("{}", fixture_driver::render_live_comparison(&runs));
        let pair_dir = pair.repeat_path();
        print!("{}", bundle::render_evidence(&pair_dir)?);
    }
    Ok(())
}

async fn run_context_mech_live(
    only: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = context_mech::load_pack()?;
    eprintln!("{}", context_mech::check_pack(&pack)?);
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir
        .unwrap_or_else(|| std::path::PathBuf::from("crates/agent-eval/evidence/context-mech"));
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    let tasks: Vec<&context_bench::BenchTask> = pack
        .tasks
        .iter()
        .filter(|task| only.as_deref().is_none_or(|id| task.id() == id))
        .collect();
    anyhow::ensure!(!tasks.is_empty(), "no matching mechanism-v2 task");
    for round in 1..=repeats {
        for task in &tasks {
            eprintln!(
                "== context-mech {} A/C repeat {round}/{repeats} ==",
                task.id()
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                task.id().to_string(),
                round,
                repeats,
                true,
            );
            let runs = fixture_driver::compare_mech_live(
                &pack,
                task,
                dir.path(),
                model.clone(),
                Some(&pair),
            )
            .await?;
            print!("{}", fixture_driver::render_live_comparison(&runs));
            let pair_dir = pair.repeat_path();
            print!("{}", bundle::render_evidence(&pair_dir)?);
        }
    }
    Ok(())
}

/// Long-flow diagnostic: same shape as the mech runner, but engines run
/// concurrently and the pack/evidence identity is longflow-specific.
async fn run_longflow_live(
    only: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = longflow::load_pack()?;
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir
        .unwrap_or_else(|| std::path::PathBuf::from("crates/agent-eval/evidence/longflow"));
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    let tasks: Vec<&context_bench::BenchTask> = pack
        .tasks
        .iter()
        .filter(|task| only.as_deref().is_none_or(|id| task.id() == id))
        .collect();
    anyhow::ensure!(!tasks.is_empty(), "no matching longflow task");
    for round in 1..=repeats {
        for task in &tasks {
            eprintln!(
                "== longflow {} A/C (concurrent) repeat {round}/{repeats} ==",
                task.id()
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                task.id().to_string(),
                round,
                repeats,
                true,
            );
            let runs = fixture_driver::compare_mech_live_parallel(
                &pack,
                task,
                dir.path(),
                model.clone(),
                Some(&pair),
                longflow::SCHEMA,
                longflow::spec_sha256(),
            )
            .await?;
            print!("{}", fixture_driver::render_live_comparison(&runs));
            let pair_dir = pair.repeat_path();
            print!("{}", bundle::render_evidence(&pair_dir)?);
        }
    }
    Ok(())
}

/// retry_policy_dev live pilot (LONG_TASK_EVALUATION.md layer 2): the C
/// engine on the frozen one-directive fixture, normal/resume with repeats.
async fn run_long_task_live(
    mode_filter: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let modes = long_live::PilotMode::parse(mode_filter.as_deref())?;
    anyhow::ensure!((1..=8).contains(&repeats), "repeats must be 1..=8");
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir
        .unwrap_or_else(|| std::path::PathBuf::from("crates/agent-eval/evidence/retry-pilot"));
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());
    for repeat in 1..=repeats {
        for mode in &modes {
            eprintln!(
                "== retry_policy_dev {} live (C) repeat {repeat}/{repeats} ==",
                mode.id()
            );
            let dir = tempfile::tempdir()?;
            let pair = bundle::PairSink::claim(
                evidence_root.clone(),
                format!("retry_policy_dev-{}", mode.id()),
                repeat,
                repeats,
                true,
            );
            let outcome =
                long_live::run_cell(*mode, &pair, model.clone(), dir.path(), false).await?;
            println!("{}", outcome.render_line());
            for violation in &outcome.diff_violations {
                println!("diff: {violation}");
            }
            for violation in &outcome.marker_violations {
                println!("marker: {violation}");
            }
            // Best effort: a fresh evidence file can transiently fail to
            // open on Windows (defender/indexer); never lose the run over it.
            match bundle::render_evidence(&pair.repeat_path()) {
                Ok(rendered) => println!("{rendered}"),
                Err(e) => eprintln!("warning: evidence render failed: {e}"),
            }
        }
    }
    Ok(())
}

/// ROADMAP item-8 off/on paired live gate for the advisory
/// completion-opportunity candidate. The switch is the only variable:
/// identical fixture, model/provider, tool surface and substrate, with
/// normal/resume repeats in both arms. Evidence lands per cell with its
/// recorded setting; the runner prints paired facts and leaves promotion
/// judgment to the REPORT.
async fn run_opportunity_gate(
    mode_filter: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let modes = long_live::PilotMode::parse(mode_filter.as_deref())?;
    anyhow::ensure!((1..=4).contains(&repeats), "repeats must be 1..=4");
    let model = driver::build_live_coding_model()?;
    let evidence_root = evidence_dir
        .unwrap_or_else(|| std::path::PathBuf::from("crates/agent-eval/evidence/opportunity-gate"));
    std::fs::create_dir_all(&evidence_root)?;
    eprintln!("evidence dir: {}", evidence_root.display());

    let mut outcomes = Vec::new();
    for opportunity in [false, true] {
        for repeat in 1..=repeats {
            for mode in &modes {
                eprintln!(
                    "== retry_policy_dev {} live (C, opp={}) repeat {repeat}/{repeats} ==",
                    mode.id(),
                    if opportunity { "on" } else { "off" },
                );
                let dir = tempfile::tempdir()?;
                let pair = bundle::PairSink::claim(
                    evidence_root.clone(),
                    format!(
                        "retry_policy_dev-{}-{}",
                        mode.id(),
                        if opportunity { "on" } else { "off" }
                    ),
                    repeat,
                    repeats,
                    true,
                );
                let outcome =
                    long_live::run_cell(*mode, &pair, model.clone(), dir.path(), opportunity)
                        .await?;
                println!("{}", outcome.render_line());
                match bundle::render_evidence(&pair.repeat_path()) {
                    Ok(rendered) => println!("{rendered}"),
                    Err(e) => eprintln!("warning: evidence render failed: {e}"),
                }
                outcomes.push(outcome);
            }
        }
    }

    println!("\n== paired summary (off vs on; facts only) ==");
    for mode in &modes {
        for (label, switch) in [("off", false), ("on", true)] {
            let cells: Vec<&long_live::CellOutcome> = outcomes
                .iter()
                .filter(|cell| cell.mode == *mode && cell.opportunity == switch)
                .collect();
            if cells.is_empty() {
                continue;
            }
            let mut rounds: Vec<u64> = cells
                .iter()
                .map(|cell| (cell.model_rounds_phase_one + cell.model_rounds_phase_two) as u64)
                .collect();
            let median_rounds = checked_percentile(&mut rounds, 50).unwrap_or(0);
            println!(
                "{:<6} opp={} cells={} passed={} median_total_rounds={} offers={} called={} completed={}",
                mode.id(),
                label,
                cells.len(),
                cells.iter().filter(|cell| cell.passed).count(),
                median_rounds,
                cells
                    .iter()
                    .map(|cell| cell.opportunity_offers.len())
                    .sum::<usize>(),
                cells.iter().filter(|cell| cell.opportunity_called).count(),
                cells
                    .iter()
                    .filter(|cell| cell.closure == "completed")
                    .count(),
            );
        }
    }
    println!("promotion judgment belongs to the evidence REPORT, not this runner");
    Ok(())
}

async fn tool_edit_model_spec() -> anyhow::Result<agent_contracts::ToolSpec> {
    use agent_contracts::ToolDispatcher as _;

    let dir = tempfile::tempdir()?;
    let workspace = agent_workspace::Workspace::open(dir.path()).await?;
    let dispatcher = tool_runtime::BuiltinToolDispatcher::new(workspace);
    dispatcher
        .specs()
        .into_iter()
        .find(|spec| spec.name == "edit.patch")
        .map(agent_contracts::ToolSpec::compact_for_model_surface)
        .ok_or_else(|| anyhow::anyhow!("production surface omitted edit.patch"))
}

fn serialized_sha256(value: &impl serde::Serialize) -> anyhow::Result<String> {
    use sha2::{Digest as _, Sha256};

    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn checked_percentile(samples: &mut [u64], percentile: u64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(metrics::percentile(samples, percentile))
}

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

fn model_identity(model: Option<&str>, base_url: Option<&str>) -> Option<String> {
    let model = model.map(str::trim).filter(|value| !value.is_empty())?;
    let base_url = match base_url {
        Some(value) => value.trim(),
        None => DEFAULT_OPENAI_BASE_URL,
    };
    if base_url.is_empty() {
        return None;
    }
    Some(format!("{base_url}\n{model}"))
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
struct ToolEditCellIdentityChecks {
    manifest_schema_matches: bool,
    fixture_id_matches: bool,
    engine_is_production: bool,
    repeat_matches: bool,
    repeats_match_plan: bool,
    live: bool,
    fixture_sha256_matches_plan: bool,
    source_tree_digest_present: bool,
    source_tree_digest_matches_plan: bool,
    model_identity_nonempty: bool,
    model_identity_matches_plan: bool,
}

impl ToolEditCellIdentityChecks {
    fn passed(self) -> bool {
        self.manifest_schema_matches
            && self.fixture_id_matches
            && self.engine_is_production
            && self.repeat_matches
            && self.repeats_match_plan
            && self.live
            && self.fixture_sha256_matches_plan
            && self.source_tree_digest_present
            && self.source_tree_digest_matches_plan
            && self.model_identity_nonempty
            && self.model_identity_matches_plan
    }
}

fn tool_edit_cell_identity_checks(
    manifest: &bundle::CellManifest,
    fixture_id: &str,
    repeat: u32,
    repeats: u32,
    fixture_sha256: &str,
    source_tree_digest: &str,
    planned_model_identity: &str,
) -> ToolEditCellIdentityChecks {
    let actual_model_identity = model_identity(
        manifest.openai_model.as_deref(),
        manifest.openai_base_url.as_deref(),
    );
    ToolEditCellIdentityChecks {
        manifest_schema_matches: manifest.schema == bundle::CELL_SCHEMA,
        fixture_id_matches: manifest.fixture_id == fixture_id,
        engine_is_production: manifest.engine == "production",
        repeat_matches: manifest.repeat == repeat,
        repeats_match_plan: manifest.repeats == repeats,
        live: manifest.live,
        fixture_sha256_matches_plan: manifest.fixture_sha256 == fixture_sha256,
        source_tree_digest_present: manifest.source_tree_digest.is_some(),
        source_tree_digest_matches_plan: manifest.source_tree_digest.as_deref()
            == Some(source_tree_digest),
        model_identity_nonempty: actual_model_identity.is_some(),
        model_identity_matches_plan: actual_model_identity.as_deref()
            == Some(planned_model_identity),
    }
}

#[derive(Debug)]
struct ToolEditRecord {
    cell: String,
    conflict: tool_edit_pack::ConflictContract,
    gate: tool_edit_gate::ToolEditGateReport,
    summary: bundle::CellSummary,
    manifest: bundle::CellManifest,
    identity: ToolEditCellIdentityChecks,
}

#[derive(Clone, Copy, Debug)]
struct ProviderUsageCell {
    tokens: Option<u64>,
    usage_incomplete: bool,
    tokens_lower_bound: bool,
}

impl ProviderUsageCell {
    fn from_summary(summary: &bundle::CellSummary) -> Self {
        Self {
            tokens: summary
                .metrics
                .get("provider_tokens_total")
                .and_then(serde_json::Value::as_u64),
            usage_incomplete: summary.usage_incomplete,
            tokens_lower_bound: summary.provider_tokens_lower_bound,
        }
    }
}

#[derive(Debug, serde::Serialize)]
struct ProviderUsageRollup {
    usage_complete_cells: usize,
    usage_incomplete_cells: usize,
    provider_tokens_lower_bound_cells: usize,
    provider_tokens_lower_bound: bool,
    provider_tokens_total: u64,
    provider_token_percentile_samples: usize,
    provider_tokens_p50: Option<u64>,
    provider_tokens_p95: Option<u64>,
}

fn provider_usage_rollup(
    cells: impl IntoIterator<Item = ProviderUsageCell>,
) -> ProviderUsageRollup {
    let mut usage_incomplete_cells = 0usize;
    let mut provider_tokens_lower_bound_cells = 0usize;
    let mut provider_tokens_total = 0u64;
    let mut complete_samples = Vec::new();
    for cell in cells {
        let usage_incomplete = cell.usage_incomplete || cell.tokens.is_none();
        let tokens_lower_bound = cell.tokens_lower_bound || usage_incomplete;
        usage_incomplete_cells += usize::from(usage_incomplete);
        provider_tokens_lower_bound_cells += usize::from(tokens_lower_bound);
        if let Some(tokens) = cell.tokens {
            provider_tokens_total = provider_tokens_total.saturating_add(tokens);
            if !usage_incomplete && !tokens_lower_bound {
                complete_samples.push(tokens);
            }
        }
    }
    let usage_complete_cells = complete_samples.len();
    let provider_tokens_p50 = checked_percentile(&mut complete_samples.clone(), 50);
    let provider_tokens_p95 = checked_percentile(&mut complete_samples, 95);
    ProviderUsageRollup {
        usage_complete_cells,
        usage_incomplete_cells,
        provider_tokens_lower_bound_cells,
        provider_tokens_lower_bound: provider_tokens_lower_bound_cells > 0,
        provider_tokens_total,
        provider_token_percentile_samples: usage_complete_cells,
        provider_tokens_p50,
        provider_tokens_p95,
    }
}

#[allow(clippy::too_many_arguments)]
async fn collect_tool_edit_cell(
    pack: &tool_edit_pack::ToolEditPack,
    task: &tool_edit_pack::ToolEditTask,
    round: u32,
    repeats: u32,
    evidence_root: &std::path::Path,
    model: std::sync::Arc<dyn agent_contracts::ModelTransport>,
    task_sha256: &str,
    source_tree_digest: &str,
    planned_model_identity: &str,
) -> anyhow::Result<ToolEditRecord> {
    let dir = tempfile::tempdir()?;
    let pair = bundle::PairSink::claim(
        evidence_root.to_path_buf(),
        task.id().to_string(),
        round,
        repeats,
        true,
    );
    let run =
        fixture_driver::run_tool_edit_live(pack, task, dir.path(), model, Some(&pair)).await?;
    print!(
        "{}",
        fixture_driver::render_live_comparison(std::slice::from_ref(&run))
    );
    let pair_dir = pair.repeat_path();
    print!("{}", bundle::render_evidence(&pair_dir)?);
    let cell_dir = pair.cell_dir("production");
    for required in [
        "events.jsonl",
        "gate.json",
        "manifest.json",
        "summary.json",
        "tool-edit.json",
        "verify.json",
        "workspace.json",
    ] {
        anyhow::ensure!(
            cell_dir.join(required).is_file(),
            "incomplete tool-edit cell {}: missing {required}",
            cell_dir.display()
        );
    }
    let gate: tool_edit_gate::ToolEditGateReport =
        serde_json::from_str(&std::fs::read_to_string(cell_dir.join("gate.json"))?)?;
    let summary: bundle::CellSummary =
        serde_json::from_str(&std::fs::read_to_string(cell_dir.join("summary.json"))?)?;
    let manifest: bundle::CellManifest =
        serde_json::from_str(&std::fs::read_to_string(cell_dir.join("manifest.json"))?)?;
    let identity = tool_edit_cell_identity_checks(
        &manifest,
        task.id(),
        round,
        repeats,
        task_sha256,
        source_tree_digest,
        planned_model_identity,
    );
    Ok(ToolEditRecord {
        cell: format!("{}/r{round}/production", task.id()),
        conflict: task.file.trace.conflict,
        gate,
        summary,
        manifest,
        identity,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_incomplete_tool_edit_summary(
    evidence_root: &std::path::Path,
    expected_cells: &[String],
    completed: &[ToolEditRecord],
    failed_cell: &str,
    error: &anyhow::Error,
    source_tree_digest: &str,
    gate_implementation_sha256: &str,
    edit_patch_spec_sha256: &str,
) -> anyhow::Result<()> {
    let error: String = error.to_string().chars().take(1_000).collect();
    let summary = serde_json::json!({
        "schema": "agent-eval.tool-edit-run-summary.v2",
        "verdict": "fail",
        "acceptance_eligible": false,
        "complete": false,
        "expected_cells": expected_cells.len(),
        "completed_cells": completed.len(),
        "completed_cell_ids": completed.iter().map(|record| record.cell.as_str()).collect::<Vec<_>>(),
        "failed_cell": failed_cell,
        "error": error,
        "source_tree_digest": source_tree_digest,
        "gate_schema": tool_edit_gate::SCHEMA,
        "gate_implementation_sha256": gate_implementation_sha256,
        "edit_patch_model_spec_sha256": edit_patch_spec_sha256,
    });
    std::fs::write(
        evidence_root.join("run-summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    Ok(())
}

async fn run_tool_edit_live(
    only: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    allow_dirty: bool,
) -> anyhow::Result<()> {
    bundle::require_clean_tree(allow_dirty)?;
    let pack = tool_edit_pack::load_pack()?;
    // Fail before the first provider call if fixture bytes or SHA identities
    // drifted. Live cells never repair or reinterpret the pack.
    tool_edit_pack::check_pack(&pack)?;
    let full_pack = only.is_none();
    let tasks: Vec<&tool_edit_pack::ToolEditTask> = match only {
        Some(id) => vec![
            pack.task(&id)
                .ok_or_else(|| anyhow::anyhow!("unknown tool-edit task: {id} (see --tool-edit)"))?,
        ],
        None => pack.tasks.iter().collect(),
    };
    let evidence_root = evidence_dir.unwrap_or_else(default_evidence_dir);
    if evidence_root.exists() {
        anyhow::ensure!(
            std::fs::read_dir(&evidence_root)?.next().is_none(),
            "tool-edit evidence directory must be empty: {}",
            evidence_root.display()
        );
    }
    std::fs::create_dir_all(&evidence_root)?;
    let edit_patch_spec = tool_edit_model_spec().await?;
    let edit_patch_spec_sha256 = serialized_sha256(&edit_patch_spec)?;
    let gate_implementation_sha256 = tool_edit_gate::implementation_sha256();
    let expected_cells: Vec<String> = tasks
        .iter()
        .flat_map(|task| {
            (1..=repeats).map(move |round| format!("{}/r{round}/production", task.id()))
        })
        .collect();
    let mut task_sha256s = std::collections::BTreeMap::new();
    for task in &tasks {
        task_sha256s.insert(task.id().to_string(), tool_edit_pack::task_sha256(task)?);
    }
    let source_tree_digest = bundle::source_tree_digest().ok_or_else(|| {
        anyhow::anyhow!(
            "Tool Edit live requires a source_tree_digest; git source identity is unavailable"
        )
    })?;
    let planned_model = envfile::get("OPENAI_MODEL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Tool Edit live requires an explicit non-empty OPENAI_MODEL for evidence identity"
            )
        })?;
    let planned_base_url = envfile::get("OPENAI_BASE_URL");
    let planned_model_identity =
        model_identity(Some(planned_model.as_str()), planned_base_url.as_deref())
            .ok_or_else(|| anyhow::anyhow!("Tool Edit live model identity is empty"))?;
    let plan = serde_json::json!({
        "schema": "agent-eval.tool-edit-run-plan.v2",
        "status": "planned",
        "scope": if full_pack { "full_pack" } else { "single_task" },
        "allow_dirty": allow_dirty,
        "pack_schema": tool_edit_pack::SCHEMA,
        "spec_sha256": tool_edit_pack::spec_sha256(),
        "pack_digest": tool_edit_pack::pack_digest(&pack)?,
        "gate_schema": tool_edit_gate::SCHEMA,
        "gate_implementation_sha256": gate_implementation_sha256,
        "edit_patch_model_spec_sha256": edit_patch_spec_sha256,
        "edit_patch_model_spec": edit_patch_spec,
        "source_tree_digest": source_tree_digest,
        "task_sha256s": task_sha256s,
        "model_identity": {
            "model": planned_model,
            "base_url": planned_base_url.as_deref().unwrap_or(DEFAULT_OPENAI_BASE_URL),
        },
        "task_ids": tasks.iter().map(|task| task.id()).collect::<Vec<_>>(),
        "repeats": repeats,
        "expected_cells": expected_cells,
    });
    std::fs::write(
        evidence_root.join("run-plan.json"),
        serde_json::to_string_pretty(&plan)?,
    )?;
    let model = driver::build_live_coding_model()?;
    eprintln!(
        "tool-edit pack={} gate={} tasks={} repeats={} cells={} surface=production-default engine=dynamic evidence={}",
        tool_edit_pack::SCHEMA,
        tool_edit_gate::SCHEMA,
        tasks.len(),
        repeats,
        tasks.len() as u32 * repeats,
        evidence_root.display()
    );

    let mut records = Vec::with_capacity(expected_cells.len());
    for task in &tasks {
        let task_sha256 = task_sha256s
            .get(task.id())
            .expect("every planned Tool Edit task has a SHA-256");
        for round in 1..=repeats {
            eprintln!("== tool-edit {} repeat {round}/{repeats} ==", task.id());
            let failed_cell = format!("{}/r{round}/production", task.id());
            match collect_tool_edit_cell(
                &pack,
                task,
                round,
                repeats,
                &evidence_root,
                model.clone(),
                task_sha256,
                &source_tree_digest,
                &planned_model_identity,
            )
            .await
            {
                Ok(record) => records.push(record),
                Err(error) => {
                    write_incomplete_tool_edit_summary(
                        &evidence_root,
                        &expected_cells,
                        &records,
                        &failed_cell,
                        &error,
                        &source_tree_digest,
                        &gate_implementation_sha256,
                        &edit_patch_spec_sha256,
                    )?;
                    return Err(error.context(format!("Tool Edit cell {failed_cell} failed")));
                }
            }
        }
    }

    let completed_cells: std::collections::BTreeSet<&str> =
        records.iter().map(|record| record.cell.as_str()).collect();
    let expected_cell_set: std::collections::BTreeSet<&str> =
        expected_cells.iter().map(String::as_str).collect();
    let exact_cell_set = completed_cells == expected_cell_set;
    let identity_checks = ToolEditCellIdentityChecks {
        manifest_schema_matches: records
            .iter()
            .all(|record| record.identity.manifest_schema_matches),
        fixture_id_matches: records
            .iter()
            .all(|record| record.identity.fixture_id_matches),
        engine_is_production: records
            .iter()
            .all(|record| record.identity.engine_is_production),
        repeat_matches: records.iter().all(|record| record.identity.repeat_matches),
        repeats_match_plan: records
            .iter()
            .all(|record| record.identity.repeats_match_plan),
        live: records.iter().all(|record| record.identity.live),
        fixture_sha256_matches_plan: records
            .iter()
            .all(|record| record.identity.fixture_sha256_matches_plan),
        source_tree_digest_present: records
            .iter()
            .all(|record| record.identity.source_tree_digest_present),
        source_tree_digest_matches_plan: records
            .iter()
            .all(|record| record.identity.source_tree_digest_matches_plan),
        model_identity_nonempty: records
            .iter()
            .all(|record| record.identity.model_identity_nonempty),
        model_identity_matches_plan: records
            .iter()
            .all(|record| record.identity.model_identity_matches_plan),
    };
    let model_identities: std::collections::BTreeSet<String> = records
        .iter()
        .filter_map(|record| {
            model_identity(
                record.manifest.openai_model.as_deref(),
                record.manifest.openai_base_url.as_deref(),
            )
        })
        .collect();
    let model_identity_unique = model_identities.len() == 1;
    let identity_passed = identity_checks.passed() && model_identity_unique;
    let source_identity_consistent = identity_checks.source_tree_digest_present
        && identity_checks.source_tree_digest_matches_plan;
    let strict_passed = records
        .iter()
        .filter(|record| record.gate.strict_passed)
        .count();
    let gate_passed = records.iter().filter(|record| record.gate.passed).count();
    let non_conflict_cells = records
        .iter()
        .filter(|record| record.conflict == tool_edit_pack::ConflictContract::None)
        .count();
    let non_conflict_first_attempt_passed = records
        .iter()
        .filter(|record| {
            record.conflict == tool_edit_pack::ConflictContract::None
                && record.gate.valid_call_first_attempt_success
        })
        .count();
    let stale_proactive = records
        .iter()
        .filter(|record| {
            record.conflict == tool_edit_pack::ConflictContract::StaleOrRevalidated
                && record.gate.passed
                && record.gate.conflict_route.as_deref() == Some("proactive")
        })
        .count();
    let stale_reactive = records
        .iter()
        .filter(|record| {
            record.conflict == tool_edit_pack::ConflictContract::StaleOrRevalidated
                && record.gate.passed
                && record.gate.conflict_route.as_deref() == Some("reactive")
        })
        .count();
    let mut wall_samples: Vec<u64> = records
        .iter()
        .map(|record| record.summary.wall_ms)
        .collect();
    let wall_ms_total = wall_samples.iter().copied().fold(0u64, u64::saturating_add);
    let wall_ms_p50 = checked_percentile(&mut wall_samples.clone(), 50);
    let wall_ms_p95 = checked_percentile(&mut wall_samples, 95);
    let provider_usage = provider_usage_rollup(
        records
            .iter()
            .map(|record| ProviderUsageCell::from_summary(&record.summary)),
    );
    let sum_gate = |project: fn(&tool_edit_gate::ToolEditGateReport) -> u64| {
        records.iter().fold(0_u64, |total, record| {
            total.saturating_add(project(&record.gate))
        })
    };
    let passed = exact_cell_set && identity_passed && gate_passed == expected_cells.len();
    let usage_complete =
        provider_usage.usage_incomplete_cells == 0 && !provider_usage.provider_tokens_lower_bound;
    let acceptance_eligible = full_pack
        && repeats == tool_edit_pack::DEFAULT_REPEATS
        && !allow_dirty
        && usage_complete
        && passed;
    let models: std::collections::BTreeSet<String> = records
        .iter()
        .filter_map(|record| record.manifest.openai_model.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    let base_urls: std::collections::BTreeSet<String> = records
        .iter()
        .filter_map(|record| record.manifest.openai_base_url.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    let cells: Vec<serde_json::Value> = records
        .iter()
        .map(|record| {
            let provider_usage = ProviderUsageCell::from_summary(&record.summary);
            serde_json::json!({
                "cell": record.cell,
                "conflict": record.conflict,
                "strict_passed": record.gate.strict_passed,
                "gate_passed": record.gate.passed,
                "first_attempt_passed": record.gate.valid_call_first_attempt_success,
                "first_patch_exact_hunks": record.gate.first_patch_exact_hunks,
                "fixture_mutation_evidence_valid": record.gate.fixture_mutation_evidence_valid,
                "conflict_route": record.gate.conflict_route,
                "patch_attempts": record.gate.patch_attempts,
                "patch_failures": record.gate.patch_failures,
                "rounds": record.gate.model_rounds,
                "wall_ms": record.summary.wall_ms,
                "provider_tokens_total": provider_usage.tokens,
                "usage_incomplete": provider_usage.usage_incomplete,
                "provider_tokens_lower_bound": provider_usage.tokens_lower_bound,
                "identity": record.identity,
                "identity_passed": record.identity.passed(),
                "violations": record.gate.violations,
            })
        })
        .collect();
    let identity_summary = serde_json::json!({
        "passed": identity_passed,
        "manifest_schema_matches": identity_checks.manifest_schema_matches,
        "fixture_id_matches": identity_checks.fixture_id_matches,
        "engine_is_production": identity_checks.engine_is_production,
        "repeat_matches": identity_checks.repeat_matches,
        "repeats_match_plan": identity_checks.repeats_match_plan,
        "live": identity_checks.live,
        "fixture_sha256_matches_plan": identity_checks.fixture_sha256_matches_plan,
        "source_tree_digest_present": identity_checks.source_tree_digest_present,
        "source_tree_digest_matches_plan": identity_checks.source_tree_digest_matches_plan,
        "model_identity_nonempty": identity_checks.model_identity_nonempty,
        "model_identity_matches_plan": identity_checks.model_identity_matches_plan,
        "model_identity_unique": model_identity_unique,
    });
    let mut run_summary = serde_json::json!({
        "schema": "agent-eval.tool-edit-run-summary.v2",
        "verdict": if passed { if acceptance_eligible { "acceptance_pass" } else { "diagnostic_pass" } } else { "fail" },
        "acceptance_eligible": acceptance_eligible,
        "usage_complete": usage_complete,
        "complete": exact_cell_set,
        "identity_passed": identity_passed,
        "identity": identity_summary,
        "source_identity_consistent": source_identity_consistent,
        "source_tree_digest": source_tree_digest,
        "task_sha256s": task_sha256s,
        "expected_cells": expected_cells.len(),
        "completed_cells": records.len(),
        "strict_passed": strict_passed,
        "gate_passed": gate_passed,
        "non_conflict_first_attempt_passed": non_conflict_first_attempt_passed,
        "non_conflict_cells": non_conflict_cells,
        "stale_proactive_passed": stale_proactive,
        "stale_reactive_passed": stale_reactive,
    });
    let summary_metrics = serde_json::json!({
        "patch_attempts": sum_gate(|gate| gate.patch_attempts),
        "patch_failures": sum_gate(|gate| gate.patch_failures),
        "stale_refusals": sum_gate(|gate| gate.stale_refusals),
        "non_stale_failures": sum_gate(|gate| gate.non_stale_failures),
        "patch_revision_provenance_failures": sum_gate(|gate| gate.patch_revision_provenance_failures),
        "patch_target_mismatches": sum_gate(|gate| gate.patch_target_mismatches),
        "patch_hunk_contract_failures": sum_gate(|gate| gate.patch_hunk_contract_failures),
        "read_identity_failures": sum_gate(|gate| gate.read_identity_failures),
        "forbidden_calls": sum_gate(|gate| gate.forbidden_calls),
        "confirm_reads_after_success": sum_gate(|gate| gate.confirm_reads_after_success),
        "commit_not_applied": sum_gate(|gate| gate.commit_not_applied),
        "commit_recovery_required": sum_gate(|gate| gate.commit_recovery_required),
        "commit_unknown": sum_gate(|gate| gate.commit_unknown),
        "model_rounds": sum_gate(|gate| gate.model_rounds),
        "fs_read_bytes": sum_gate(|gate| gate.fs_read_bytes),
        "wall_ms_total": wall_ms_total,
        "wall_ms_p50": wall_ms_p50,
        "wall_ms_p95": wall_ms_p95,
        "usage_complete_cells": provider_usage.usage_complete_cells,
        "usage_incomplete_cells": provider_usage.usage_incomplete_cells,
        "provider_tokens_lower_bound_cells": provider_usage.provider_tokens_lower_bound_cells,
        "provider_tokens_lower_bound": provider_usage.provider_tokens_lower_bound,
        "provider_tokens_total": provider_usage.provider_tokens_total,
        "provider_token_percentile_samples": provider_usage.provider_token_percentile_samples,
        "provider_tokens_p50": provider_usage.provider_tokens_p50,
        "provider_tokens_p95": provider_usage.provider_tokens_p95,
        "models": models,
        "base_urls": base_urls,
        "model_identities": model_identities,
        "gate_schema": tool_edit_gate::SCHEMA,
        "gate_implementation_sha256": gate_implementation_sha256,
        "edit_patch_model_spec_sha256": edit_patch_spec_sha256,
        "cells": cells,
    });
    run_summary
        .as_object_mut()
        .expect("Tool Edit run summary is an object")
        .extend(
            summary_metrics
                .as_object()
                .expect("Tool Edit metric summary is an object")
                .clone(),
        );
    std::fs::write(
        evidence_root.join("run-summary.json"),
        serde_json::to_string_pretty(&run_summary)?,
    )?;
    println!(
        "tool-edit run verdict={} identity={} strict={}/{} gate={}/{} non_conflict_first={}/{} stale proactive/reactive={}/{} rounds={} wall_ms={} tokens={} lower_bound={} usage_incomplete_cells={}",
        run_summary["verdict"].as_str().unwrap_or("fail"),
        identity_passed,
        strict_passed,
        expected_cells.len(),
        gate_passed,
        expected_cells.len(),
        non_conflict_first_attempt_passed,
        non_conflict_cells,
        stale_proactive,
        stale_reactive,
        run_summary["model_rounds"],
        wall_ms_total,
        provider_usage.provider_tokens_total,
        provider_usage.provider_tokens_lower_bound,
        provider_usage.usage_incomplete_cells,
    );
    anyhow::ensure!(
        passed,
        "Tool Edit gate failed after writing complete evidence to {}",
        evidence_root.display()
    );
    Ok(())
}

fn default_evidence_dir() -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::path::PathBuf::from("target/eval-evidence").join(secs.to_string())
}

#[cfg(test)]
mod tool_edit_main_tests {
    use super::*;

    fn manifest() -> bundle::CellManifest {
        bundle::CellManifest {
            schema: bundle::CELL_SCHEMA.into(),
            fixture_id: "fixture".into(),
            engine: "production".into(),
            repeat: 1,
            repeats: 3,
            live: true,
            tool_surface: Some("production".into()),
            fixture_sha256: "fixture-sha".into(),
            git_head: None,
            git_dirty: None,
            git_dirty_sha256: None,
            source_tree_digest: Some("source-sha".into()),
            openai_model: Some("model".into()),
            openai_base_url: None,
        }
    }

    #[test]
    fn tool_edit_identity_checks_every_manifest_binding() {
        let valid = manifest();
        let planned_model = model_identity(Some("model"), None).unwrap();
        let checks = tool_edit_cell_identity_checks(
            &valid,
            "fixture",
            1,
            3,
            "fixture-sha",
            "source-sha",
            &planned_model,
        );
        assert!(checks.passed());

        let invalid = bundle::CellManifest {
            schema: "wrong-schema".into(),
            fixture_id: "wrong-fixture".into(),
            engine: "dynamic".into(),
            repeat: 2,
            repeats: 4,
            live: false,
            fixture_sha256: "wrong-fixture-sha".into(),
            source_tree_digest: None,
            openai_model: Some(" ".into()),
            ..valid
        };
        let checks = tool_edit_cell_identity_checks(
            &invalid,
            "fixture",
            1,
            3,
            "fixture-sha",
            "source-sha",
            &planned_model,
        );
        assert!(!checks.manifest_schema_matches);
        assert!(!checks.fixture_id_matches);
        assert!(!checks.engine_is_production);
        assert!(!checks.repeat_matches);
        assert!(!checks.repeats_match_plan);
        assert!(!checks.live);
        assert!(!checks.fixture_sha256_matches_plan);
        assert!(!checks.source_tree_digest_present);
        assert!(!checks.source_tree_digest_matches_plan);
        assert!(!checks.model_identity_nonempty);
        assert!(!checks.model_identity_matches_plan);
        assert!(!checks.passed());
    }

    #[test]
    fn provider_usage_excludes_incomplete_and_lower_bound_cells_from_percentiles() {
        let rollup = provider_usage_rollup([
            ProviderUsageCell {
                tokens: Some(100),
                usage_incomplete: false,
                tokens_lower_bound: false,
            },
            ProviderUsageCell {
                tokens: Some(200),
                usage_incomplete: true,
                tokens_lower_bound: true,
            },
            ProviderUsageCell {
                tokens: Some(300),
                usage_incomplete: false,
                tokens_lower_bound: true,
            },
            ProviderUsageCell {
                tokens: None,
                usage_incomplete: false,
                tokens_lower_bound: false,
            },
        ]);
        assert_eq!(rollup.provider_tokens_total, 600);
        assert_eq!(rollup.usage_complete_cells, 1);
        assert_eq!(rollup.usage_incomplete_cells, 2);
        assert_eq!(rollup.provider_tokens_lower_bound_cells, 3);
        assert!(rollup.provider_tokens_lower_bound);
        assert_eq!(rollup.provider_token_percentile_samples, 1);
        assert_eq!(rollup.provider_tokens_p50, Some(100));
        assert_eq!(rollup.provider_tokens_p95, Some(100));
    }

    #[test]
    fn provider_usage_without_complete_samples_reports_unavailable_percentiles() {
        let rollup = provider_usage_rollup([ProviderUsageCell {
            tokens: None,
            usage_incomplete: true,
            tokens_lower_bound: true,
        }]);
        assert_eq!(rollup.provider_tokens_p50, None);
        assert_eq!(rollup.provider_tokens_p95, None);
        assert_eq!(checked_percentile(&mut [], 50), None);
    }
}

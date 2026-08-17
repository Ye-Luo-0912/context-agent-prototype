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

mod acceptance;
mod analysis;
mod bundle;
mod context_bench;
mod driver;
mod envfile;
mod fixture_driver;
mod harvest;
mod metrics;
mod mock_model;
mod pilot;
mod retrieval;
mod suite;
mod task;
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
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live <id>\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live-all\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --compare-live-reasonable\n\
         \n\
         Same three engines and hidden verification as --compare-arm, but\n\
         each cell uses a real model (OPENAI_*) on a fresh workspace.\n\
         Each fixture sends its live_turns (one prompt for the original\n\
         four; five for recall_after_fix). Tool-loop capped. Put --repeats\n\
         first (1..=8). --compare-live-reasonable runs add_test plus\n\
         recall_after_fix. Live cells always write a versioned evidence\n\
         bundle (default target/eval-evidence/<unix-secs>). This is the\n\
         live paired smoke, not the 300×3 gate.\n\
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
         usage: agent-eval --context-bench\n\
         usage: agent-eval [--repeats N] [--evidence-dir <dir>] --context-bench-run [id]\n\
         \n\
         EVAL-02 Context Benchmark: 12 tasks that ask where dynamic context\n\
         helps or hurts a coding agent. --context-bench prints the pack.\n\
         --context-bench-run is live A/C (rolling only on horizon_long,\n\
         semantic_recall, task_switch). Wave 1 is 24+3 cells at repeats=1.\n\
         This does not close M15 and does not open the 300×3 ITT gate.\n"
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
    let mut evidence_dir: Option<std::path::PathBuf> = None;
    let mut include_swebench = false;
    let mut file_only = false;
    let mut pilot_id: Option<String> = None;

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
                run_pilot_live(pilot_id, repeats, evidence_dir, include_swebench).await?;
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
                let fixture = workload::FIXTURES
                    .iter()
                    .find(|fixture| fixture.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown fixture: {id} (see --fixtures)"))?;
                run_live_compare(&[fixture], repeats, evidence_dir).await?;
                return Ok(());
            }
            "--compare-live-all" => {
                let fixtures: Vec<&workload::CodingFixture> = workload::FIXTURES.iter().collect();
                run_live_compare(&fixtures, repeats, evidence_dir).await?;
                return Ok(());
            }
            "--compare-live-reasonable" => {
                let fixtures: Vec<&workload::CodingFixture> = workload::FIXTURES
                    .iter()
                    .filter(|fixture| fixture.id == "add_test" || fixture.id == "recall_after_fix")
                    .collect();
                run_live_compare(&fixtures, repeats, evidence_dir).await?;
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
            "--context-bench" => {
                let pack = context_bench::load_pack()?;
                print!("{}", context_bench::render_pack(&pack));
                print!("{}", context_bench::check_pack(&pack)?);
                return Ok(());
            }
            "--context-bench-run" => {
                let only = args.next().filter(|value| !value.starts_with('-'));
                run_context_bench_live(only, repeats, evidence_dir).await?;
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
) -> anyhow::Result<()> {
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
            let pair = bundle::PairSink {
                root: evidence_root.clone(),
                fixture_id: fixture.id.to_string(),
                repeat: round,
                repeats,
                live: true,
            };
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
            print!(
                "{}",
                bundle::render_evidence(&pair.root.join(fixture.id).join(format!("r{round}")))?
            );
        }
    }
    Ok(())
}

async fn run_pilot_live(
    only_id: Option<String>,
    repeats: u32,
    evidence_dir: Option<std::path::PathBuf>,
    include_swebench: bool,
) -> anyhow::Result<()> {
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
            let pair = bundle::PairSink {
                root: evidence_root.clone(),
                fixture_id: task.id.clone(),
                repeat: round,
                repeats,
                live: true,
            };
            let runs =
                fixture_driver::compare_suite_live(task, dir.path(), model.clone(), Some(&pair))
                    .await?;
            println!("task {} repeat {round}/{repeats}", task.id);
            print!("{}", fixture_driver::render_live_comparison(&runs));
            print!(
                "{}",
                bundle::render_evidence(&pair.root.join(&task.id).join(format!("r{round}")))?
            );
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
) -> anyhow::Result<()> {
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
            let pair = bundle::PairSink {
                root: evidence_root.clone(),
                fixture_id: task.id().to_string(),
                repeat: round,
                repeats,
                live: true,
            };
            let runs = fixture_driver::compare_bench_live(
                &pack,
                task,
                dir.path(),
                model.clone(),
                Some(&pair),
            )
            .await?;
            print!("{}", fixture_driver::render_live_comparison(&runs));
            let pair_dir = pair.root.join(task.id()).join(format!("r{round}"));
            print!("{}", bundle::render_evidence(&pair_dir)?);
        }
    }
    Ok(())
}

fn default_evidence_dir() -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::path::PathBuf::from("target/eval-evidence").join(secs.to_string())
}

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

mod analysis;
mod bundle;
mod driver;
mod envfile;
mod fixture_driver;
mod metrics;
mod mock_model;
mod retrieval;
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
         usage: agent-eval --preregister\n\
         \n\
         Print the frozen EVAL-01.3 analysis spec, spec hash, and power\n\
         simulation (historical 30×3 plus the 300×3 design). The acceptance\n\
         suite is not frozen (5 fixtures). This does not close M15.\n\
         \n\
         usage: agent-eval --analyze-evidence <dir>\n\
         \n\
         Rebuild the predeclared C-A clustered interval from EVAL-01.1\n\
         bundles. A result is ineligible until the 300-task suite is frozen.\n\
         \n\
         usage: agent-eval --retrieval\n\
         \n\
         Engine-only retrieval baseline: GC-externalize unique facts, then\n\
         measure search recall/latency and graded access stamps. Not the\n\
         paired real-model coding gate.\n"
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

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => {
                workload::verify_fixture_inputs()?;
                print!("{}", workload::render_fixtures());
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
                print!("fixture {} repeat {round}/{repeats}\n", fixture.id);
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

fn default_evidence_dir() -> std::path::PathBuf {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::path::PathBuf::from("target/eval-evidence").join(secs.to_string())
}

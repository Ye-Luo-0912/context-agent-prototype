//! Headless live evaluation: run a context-retention workload through the
//! A/B/C context engines with a real model (OpenAI-compatible, configured
//! via `OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OPENAI_MODEL`) and compare
//! token cost against task success.
//!
//! The task needs no tools (the live provider endpoint rejects requests
//! that carry a `tools` array): five constraints are stated up front, six
//! turns of unrelated noise follow, and the final turn asks for the
//! constraints. A model can only answer from what its context frame
//! retained, so the comparison measures the context policy directly.

mod driver;
mod metrics;
mod task;
mod workload;

fn usage() -> ! {
    eprintln!(
        "usage: agent-eval [--engine append|rolling|dynamic] [--all]\n\
         \n\
         Runs the constraint-retention task through the selected context\n\
         engine(s) with a real model and prints token cost vs. task success.\n\
         Requires OPENAI_API_KEY (and optionally OPENAI_BASE_URL / OPENAI_MODEL).\n\
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
         calling a model.\n"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut engines: Vec<&'static str> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => {
                workload::verify_fixture_inputs()?;
                print!("{}", workload::render_fixtures());
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

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
mod task;

fn usage() -> ! {
    eprintln!(
        "usage: agent-eval [--engine append|rolling|dynamic] [--all]\n\
         \n\
         Runs the constraint-retention task through the selected context\n\
         engine(s) with a real model and prints token cost vs. task success.\n\
         Requires OPENAI_API_KEY (and optionally OPENAI_BASE_URL / OPENAI_MODEL).\n"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut engines: Vec<&'static str> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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

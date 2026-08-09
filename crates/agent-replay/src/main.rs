use std::path::Path;

use agent_replay::{ReplayConfig, compare_config, render_comparison, render_report, replay_file};

fn usage() -> ! {
    eprintln!(
        "usage: agent-replay <trace.jsonl> [--system-prompt <text>] [--budget <tokens>]\n\
         \n\
         Replays a context lifecycle from a JSONL event journal produced by the\n\
         runtime (one RuntimeEventEnvelope per line) and prints a per-item report:\n\
         what entered, why, which turns consumed it, when/why it left, and its\n\
         final state.\n\
         \n\
         A/B/C comparison modes:\n\
         \n\
         usage: agent-replay --compare [scenario]\n\
         \n\
         Runs the named scenario (or all seven) through the three context\n\
         policies — A append-only, B rolling summary, C dynamic working set —\n\
         and prints a metrics table: total/max input tokens, over-budget\n\
         snapshots, lifecycle churn and final working-set size.\n\
         \n\
         usage: agent-replay --facts [scenario]\n\
         \n\
         Same comparison plus key-fact coverage: which required facts stayed\n\
         in the model-visible working set when they mattered, and which\n\
         forbidden (stale) facts leaked. The completion-quality proxy that\n\
         needs no model.\n"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(first) = args.next() else {
        usage();
    };

    if first == "--compare" {
        let filter = args.next();
        let scenarios = agent_replay::all_scenarios();
        let selected: Vec<_> = match &filter {
            Some(name) => scenarios
                .into_iter()
                .filter(|scenario| scenario.name == *name)
                .collect(),
            None => scenarios,
        };
        if selected.is_empty() {
            eprintln!("unknown scenario: {}", filter.unwrap_or_default());
            usage();
        }
        let config = compare_config();
        for scenario in &selected {
            let results = agent_replay::compare_scenario(scenario, &config).await?;
            print!("{}", render_comparison(scenario, &results));
        }
        return Ok(());
    }

    if first == "--facts" {
        let filter = args.next();
        let scenarios = agent_replay::all_scenarios();
        let selected: Vec<_> = match &filter {
            Some(name) => scenarios
                .into_iter()
                .filter(|scenario| scenario.name == *name)
                .collect(),
            None => scenarios,
        };
        if selected.is_empty() {
            eprintln!("unknown scenario: {}", filter.unwrap_or_default());
            usage();
        }
        let config = compare_config();
        for scenario in &selected {
            let results = agent_replay::compare_facts(scenario, &config).await?;
            print!(
                "{}",
                agent_replay::render_fact_comparison(scenario, &results)
            );
        }
        return Ok(());
    }

    let mut config = ReplayConfig::default();
    let path = first;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--system-prompt" => {
                let Some(value) = args.next() else {
                    usage();
                };
                config.system_prompt = value;
            }
            "--budget" => {
                let Some(value) = args.next() else {
                    usage();
                };
                let Ok(tokens) = value.parse::<usize>() else {
                    usage();
                };
                config.budget_tokens = tokens;
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }

    let outcome = replay_file(Path::new(&path), &config).await?;
    print!("{}", render_report(&outcome));
    Ok(())
}

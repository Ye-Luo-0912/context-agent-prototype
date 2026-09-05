use std::path::Path;
use std::sync::Arc;

use agent_replay::{
    ReplayConfig, compare_config, recovery_replay_file, render_comparison, render_recovery_report,
    render_report, replay_file,
};

fn usage() -> ! {
    eprintln!(
        "usage: agent-replay <trace.jsonl> [--system-prompt <text>] [--budget <tokens>] [--workspace <dir>]\n\
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
         needs no model.\n\
         \n\
         usage: agent-replay --frame-report <trace.jsonl> [more.jsonl...]         
         Frame-1 offline comparison: fold ContextFrameShadow manifests and         ModelStarted prompt-layer costs out of traces captured with the         shadow_context_frame flag on, and print the per-round context-layer         vs structured-frame cost table, dedup and required-miss totals.         
         usage: agent-replay --recover <trace.jsonl> [--engine dynamic|append|rolling]\n\
         \n\
         Crash-recovery replay (CORE-02): re-read the trace to locate the\n\
         durability barrier (last committed TurnCompleted, any\n\
         TurnCommitFailed/RecoveryRequired), check the envelope sequence is\n\
         contiguous, and rebuild the context-engine state from the events —\n\
         the state a recovery can trust after a failed turn commit. The\n\
         engine kind must match the run that wrote the trace; traces do not\n\
         record it.\n"
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
        for scenario in selected {
            let config = config.clone();
            let run = scenario.clone();
            let results =
                tokio::spawn(async move { agent_replay::compare_scenario(&run, &config).await })
                    .await
                    .map_err(|error| anyhow::anyhow!("replay compare worker: {error}"))??;
            print!("{}", render_comparison(&scenario, &results));
        }
        return Ok(());
    }

    if first == "--recover" {
        let Some(path) = args.next() else {
            usage();
        };
        let mut config = ReplayConfig::default();
        let mut engine_kind = agent_replay::ReplayEngineKind::Dynamic;
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
                "--workspace" => {
                    let Some(value) = args.next() else {
                        usage();
                    };
                    let workspace = agent_workspace::Workspace::open(&value).await?;
                    config.artifact_workspace = Some(Arc::new(workspace));
                }
                "--engine" => {
                    let Some(value) = args.next() else {
                        usage();
                    };
                    engine_kind = match value.as_str() {
                        "dynamic" => agent_replay::ReplayEngineKind::Dynamic,
                        "append" => agent_replay::ReplayEngineKind::Append,
                        "rolling" => agent_replay::ReplayEngineKind::Rolling,
                        other => {
                            eprintln!("unknown engine kind: {other}");
                            usage();
                        }
                    };
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    usage();
                }
            }
        }
        let report = recovery_replay_file(Path::new(&path), &config, engine_kind).await?;
        print!("{}", render_recovery_report(&report));
        return Ok(());
    }

    if first == "--frame-report" {
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for arg in args.by_ref() {
            if arg.starts_with('-') {
                eprintln!("unknown argument: {arg}");
                usage();
            }
            paths.push(std::path::PathBuf::from(arg));
        }
        if paths.is_empty() {
            eprintln!("--frame-report needs at least one trace file");
            usage();
        }
        let report = agent_replay::frame_report_from_files(&paths)?;
        print!("{}", agent_replay::render_frame_report(&report));
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
        for scenario in selected {
            let config = config.clone();
            let run = scenario.clone();
            let results =
                tokio::spawn(async move { agent_replay::compare_facts(&run, &config).await })
                    .await
                    .map_err(|error| anyhow::anyhow!("replay facts worker: {error}"))??;
            print!(
                "{}",
                agent_replay::render_fact_comparison(&scenario, &results)
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
            "--workspace" => {
                let Some(value) = args.next() else {
                    usage();
                };
                let workspace = agent_workspace::Workspace::open(&value).await?;
                config.artifact_workspace = Some(Arc::new(workspace));
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

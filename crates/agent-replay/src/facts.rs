//! Completion-quality evaluation at replay level: key-fact coverage.
//!
//! A task only succeeds if the model can see the facts it needs when it
//! needs them. Each scenario declares a set of `KeyFact`s — a content
//! needle that must be visible to the model in a turn window (a *required*
//! fact: e.g. the latest file content, the previous failure) or must *not*
//! be visible (a *forbidden* fact: e.g. a superseded decision or a
//! completed task's detail leaking into an unrelated task). The evaluator
//! replays the scenario and, at every materialization, checks which
//! needles are in the model-visible working set.
//!
//! This is the replay-level proxy for the completion-quality metrics the
//! roadmap tracks (completion quality, repeated-mistake rate, stale
//! instruction leakage): it needs no model, so it runs in CI, and every
//! miss is explainable as "fact X was not in view on turn N".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_contracts::{ContextEngine, MaterializedItem, RuntimeEventEnvelope};

use crate::{ReplayConfig, ReplayOutcome, run_engine_observing};

/// One fact a scenario's success depends on: a content needle that must be
/// (or must not be) in the model-visible working set during `[from_turn,
/// to_turn]` (inclusive, 1-based turn ids matching the replay's turn count).
#[derive(Debug, Clone)]
pub struct KeyFact {
    /// Content fragment matched against every `MaterializedItem.content`.
    pub needle: &'static str,
    /// First turn the fact matters (inclusive).
    pub from_turn: u64,
    /// Last turn the fact matters (inclusive).
    pub to_turn: u64,
    /// Why the fact is necessary for (or harmful to) success.
    pub reason: &'static str,
    /// `true`: must be visible in every turn of the window. `false`: must
    /// never be visible in the window (stale-instruction leakage).
    pub must_see: bool,
}

/// The measured coverage of one fact.
#[derive(Debug, Clone)]
pub struct FactOutcome {
    pub needle: &'static str,
    pub reason: &'static str,
    pub must_see: bool,
    /// Window length in turns.
    pub required_turns: u64,
    /// Turns of the window in which the fact was visible (required facts)
    /// or leaked (forbidden facts).
    pub met_turns: u64,
    /// Whether the fact was violated (a required fact out of view, or a
    /// forbidden fact in view).
    pub violated: bool,
    /// First window turn where the violation is observable, for debugging.
    pub first_violation_turn: Option<u64>,
}

/// Aggregated fact coverage for one engine replay.
#[derive(Debug, Clone)]
pub struct FactCoverage {
    pub facts: Vec<FactOutcome>,
    /// Total window turns across required facts.
    pub required_turns: u64,
    /// Window turns in which a required fact was in view.
    pub required_met: u64,
    /// Number of forbidden facts that leaked at least once.
    pub forbidden_violations: usize,
    /// Required-fact coverage ratio in `[0, 1]`.
    pub coverage_ratio: f64,
}

impl FactCoverage {
    pub fn violated(&self) -> bool {
        self.coverage_ratio < 1.0 || self.forbidden_violations > 0
    }
}

/// The key facts each scenario declares. A scenario with no facts returns
/// an empty slice (its success does not hinge on retained history).
///
/// Turn windows are 1-based and match the replay's turn count. A required
/// fact's window starts at the turn *after* it was introduced: in its own
/// turn the fact rides the runtime's turn frame (the current user message
/// and tool results), so the working set only has to carry it from the
/// next turn on.
pub fn scenario_key_facts(name: &str) -> Vec<KeyFact> {
    match name {
        "long_refactor" => vec![
            KeyFact {
                needle: "fn handle_21()",
                from_turn: 22,
                to_turn: 24,
                reason: "the final refactor steps must keep the current file content in view",
                must_see: true,
            },
            KeyFact {
                needle: "FAIL ",
                from_turn: 23,
                to_turn: 23,
                reason: "the last fix must see the previous round's failure",
                must_see: true,
            },
        ],
        "test_fix_loop" => vec![KeyFact {
            needle: "FAIL ",
            from_turn: 2,
            to_turn: 16,
            reason: "every fix round must see the previous failure; the final green round must still have the last failure in view when it acts",
            must_see: true,
        }],
        // This scenario's sharpest quality claim is the contamination
        // direction: task B's detail must not leak back into task A's
        // finish. Task A's own history is intentionally not asserted —
        // resuming a task brings back what the policy promotes, and this
        // workload has no durable decisions to promote.
        "task_switch_and_return" => vec![KeyFact {
            needle: "fn log_request()",
            from_turn: 23,
            to_turn: 28,
            reason: "task B's middleware detail must not contaminate task A's finish",
            must_see: false,
        }],
        "superseded_decisions" => vec![
            KeyFact {
                needle: "flat TOML keys",
                from_turn: 8,
                to_turn: 16,
                reason: "the final decision must stay in view through implementation",
                must_see: true,
            },
            KeyFact {
                needle: "use TOML for config",
                from_turn: 7,
                to_turn: 16,
                reason: "the superseded first decision must not contaminate implementation",
                must_see: false,
            },
        ],
        "high_volume_irrelevant_output" => vec![],
        "completed_then_unrelated" => vec![
            KeyFact {
                needle: "fn export()",
                from_turn: 12,
                to_turn: 20,
                reason: "the CSV task's file must stay in view",
                must_see: true,
            },
            KeyFact {
                needle: "fn list()",
                from_turn: 11,
                to_turn: 20,
                reason: "the completed pagination detail must not contaminate the CSV task",
                must_see: false,
            },
        ],
        "pinned_constraint" => vec![KeyFact {
            needle: "Never edit files under generated/",
            from_turn: 1,
            to_turn: 15,
            reason: "the pinned constraint must be visible in every turn",
            must_see: true,
        }],
        _ => Vec::new(),
    }
}

/// Replay `events` through `engine` and measure how often each key fact is
/// in the model-visible working set during its window.
pub async fn measure_fact_coverage(
    engine: Arc<dyn ContextEngine>,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    facts: &[KeyFact],
) -> anyhow::Result<FactCoverage> {
    // visible[turn] = indices of facts whose needle appeared in at least
    // one materialized snapshot of that turn (a turn has one snapshot per
    // model round).
    let mut visible: HashMap<u64, HashSet<usize>> = HashMap::new();
    run_engine_observing(engine, events, config, |turn, items| {
        let content = render_items(items);
        for (index, fact) in facts.iter().enumerate() {
            if content.contains(fact.needle) {
                visible.entry(turn).or_default().insert(index);
            }
        }
    })
    .await?;

    let mut outcomes = Vec::with_capacity(facts.len());
    let mut required_turns = 0u64;
    let mut required_met = 0u64;
    let mut forbidden_violations = 0usize;
    for (index, fact) in facts.iter().enumerate() {
        let required_turns_for_fact = fact.to_turn - fact.from_turn + 1;
        let mut met_turns = 0u64;
        let mut first_violation_turn = None;
        for turn in fact.from_turn..=fact.to_turn {
            let seen = visible
                .get(&turn)
                .map(|set| set.contains(&index))
                .unwrap_or(false);
            if seen == fact.must_see {
                met_turns += 1;
            } else if first_violation_turn.is_none() {
                first_violation_turn = Some(turn);
            }
        }
        let violated = met_turns < required_turns_for_fact;
        if fact.must_see {
            required_turns += required_turns_for_fact;
            required_met += met_turns;
        } else if violated {
            forbidden_violations += 1;
        }
        outcomes.push(FactOutcome {
            needle: fact.needle,
            reason: fact.reason,
            must_see: fact.must_see,
            required_turns: required_turns_for_fact,
            met_turns,
            violated,
            first_violation_turn,
        });
    }

    let coverage_ratio = if required_turns == 0 {
        1.0
    } else {
        required_met as f64 / required_turns as f64
    };
    Ok(FactCoverage {
        facts: outcomes,
        required_turns,
        required_met,
        forbidden_violations,
        coverage_ratio,
    })
}

/// Replay one scenario through all three engines, collecting both the
/// token-cost outcome and the fact coverage for each.
pub async fn compare_facts(
    scenario: &crate::Scenario,
    config: &ReplayConfig,
) -> anyhow::Result<Vec<(&'static str, ReplayOutcome, FactCoverage)>> {
    let facts = scenario_key_facts(scenario.name);
    let mut results = Vec::new();
    // Outcome and fact coverage are two observations of the same scripted
    // run, but each must start from a fresh engine. Replaying both into one
    // instance accumulated the scenario twice and made A/B/C fact results
    // depend on state left by the cost measurement.
    let outcome_engines = crate::engine_variants();
    let coverage_engines = crate::engine_variants();
    for ((label, outcome_engine), (coverage_label, coverage_engine)) in
        outcome_engines.into_iter().zip(coverage_engines)
    {
        anyhow::ensure!(
            label == coverage_label,
            "engine variant order drifted between independent replay runs"
        );
        let outcome =
            run_engine_observing(outcome_engine, &scenario.events, config, |_, _| {}).await?;
        let coverage =
            measure_fact_coverage(coverage_engine, &scenario.events, config, &facts).await?;
        results.push((label, outcome, coverage));
    }
    Ok(results)
}

/// Human-readable A/B/C fact-coverage table for one scenario.
pub fn render_fact_comparison(
    scenario: &crate::Scenario,
    results: &[(&str, ReplayOutcome, FactCoverage)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "scenario: {} - {}\n",
        scenario.name, scenario.description
    ));
    let facts = scenario_key_facts(scenario.name);
    if facts.is_empty() {
        out.push_str("  (no key facts declared for this scenario)\n");
        return out;
    }
    out.push_str(&format!(
        "  {:18} {:>13} {:>10} {:>10} {:>9} {:>7}\n",
        "engine", "in_tok_total", "req_met", "req_viol", "forb_viol", "coverage"
    ));
    for (label, outcome, coverage) in results {
        let req_viol = coverage.required_turns - coverage.required_met;
        out.push_str(&format!(
            "  {:18} {:>13} {:>10} {:>10} {:>9} {:>6.1}%\n",
            label,
            outcome.input_tokens_total,
            format!("{}/{}", coverage.required_met, coverage.required_turns),
            req_viol,
            coverage.forbidden_violations,
            coverage.coverage_ratio * 100.0,
        ));
    }
    // Detail lines for every fact, so a violation is explainable.
    for (index, fact) in facts.iter().enumerate() {
        let mut detail = String::new();
        for (label, _, coverage) in results {
            let fact_outcome = &coverage.facts[index];
            let marker = if fact_outcome.violated {
                "VIOLATED"
            } else {
                "ok"
            };
            let first = fact_outcome
                .first_violation_turn
                .map(|turn| format!(" (first: turn {turn})"))
                .unwrap_or_default();
            detail.push_str(&format!("    {label}: {marker}{first}; "));
        }
        out.push_str(&format!(
            "  fact [{}] {:?} \"{}\" turns {}-{}: {}\n",
            if fact.must_see {
                "must-see"
            } else {
                "forbidden"
            },
            fact.reason,
            fact.needle,
            fact.from_turn,
            fact.to_turn,
            detail.trim_end_matches("; "),
        ));
    }
    out
}

fn render_items(items: &[MaterializedItem]) -> String {
    items
        .iter()
        .map(|item| item.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compare_config, engine_variants, scenarios::all_scenarios};

    #[tokio::test]
    async fn required_facts_are_measured_per_turn() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "test_fix_loop")
            .expect("test_fix_loop");
        let facts = scenario_key_facts(scenario.name);
        assert_eq!(facts.len(), 1);
        let coverage = measure_fact_coverage(
            engine_variants()[2].1.clone(),
            &scenario.events,
            &config,
            &facts,
        )
        .await
        .unwrap();
        assert_eq!(coverage.required_turns, 15);
        assert_eq!(
            coverage.required_met, 15,
            "every fix round sees the failure"
        );
        assert_eq!(coverage.forbidden_violations, 0);
        assert!(!coverage.violated());
    }

    #[tokio::test]
    async fn forbidden_facts_leak_in_append_only_but_not_in_dynamic() {
        let config = compare_config();
        for name in [
            "task_switch_and_return",
            "superseded_decisions",
            "completed_then_unrelated",
        ] {
            let scenario = all_scenarios()
                .into_iter()
                .find(|s| s.name == name)
                .expect(name);
            let results = compare_facts(&scenario, &config).await.unwrap();
            let append = results
                .iter()
                .find(|(l, _, _)| *l == "A append-only")
                .unwrap();
            let dynamic = results.iter().find(|(l, _, _)| *l == "C dynamic").unwrap();

            // The dynamic engine archives completed/superseded detail, so
            // nothing forbidden is in view during the later windows.
            assert_eq!(
                dynamic.2.forbidden_violations, 0,
                "{name}: the dynamic working set must not leak stale facts"
            );
            // Append-only keeps everything, so the stale detail stays in
            // view — the sharpest quality contrast, not just a token one.
            assert!(
                append.2.forbidden_violations > 0,
                "{name}: append-only must leak the stale fact"
            );
            // And the dynamic engine still keeps every required fact.
            assert!(
                !dynamic.2.violated(),
                "{name}: required facts must stay in view"
            );
            assert!(
                dynamic.1.input_tokens_total < append.1.input_tokens_total,
                "{name}: the dynamic engine must also stay cheaper"
            );
        }
    }

    #[tokio::test]
    async fn pinned_constraint_is_visible_in_every_engine() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "pinned_constraint")
            .expect("pinned_constraint");
        let results = compare_facts(&scenario, &config).await.unwrap();
        for (_, _, coverage) in results {
            assert!(
                !coverage.violated(),
                "the pinned constraint must be visible in every engine"
            );
        }
    }

    #[tokio::test]
    async fn comparison_coverage_matches_an_independent_fresh_replay() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "completed_then_unrelated")
            .expect("completed_then_unrelated");
        let facts = scenario_key_facts(scenario.name);
        let compared = compare_facts(&scenario, &config).await.unwrap();

        for ((label, _, actual), (fresh_label, fresh_engine)) in
            compared.iter().zip(engine_variants())
        {
            assert_eq!(*label, fresh_label);
            let expected = measure_fact_coverage(fresh_engine, &scenario.events, &config, &facts)
                .await
                .unwrap();
            assert_eq!(actual.required_turns, expected.required_turns);
            assert_eq!(actual.required_met, expected.required_met);
            assert_eq!(
                actual.forbidden_violations, expected.forbidden_violations,
                "{label} comparison coverage must not inherit the outcome run's state"
            );
            assert_eq!(actual.coverage_ratio, expected.coverage_ratio);
        }
    }

    #[tokio::test]
    async fn render_fact_comparison_covers_all_facts() {
        let config = compare_config();
        let scenario = all_scenarios()
            .into_iter()
            .find(|s| s.name == "completed_then_unrelated")
            .expect("completed_then_unrelated");
        let results = compare_facts(&scenario, &config).await.unwrap();
        let rendered = render_fact_comparison(&scenario, &results);
        assert!(rendered.contains("fn export()"));
        assert!(rendered.contains("fn list()"));
        assert!(rendered.contains("coverage"));
    }
}

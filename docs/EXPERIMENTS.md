# A/B/C Context Lifecycle Experiments (P3)

## 1. Purpose

The central claim of this prototype is that a continuously maintained,
task-focused working set (policy C) beats the two classic baselines on
real coding workloads:

- **A — append-only**: every message and tool result is resent every model
  turn until the window limit forces a stop.
- **B — rolling summary**: append like A, but when retained history crosses a
  token threshold, the oldest part (outside a verbatim recency window) is
  collapsed into a summary marker.
- **C — dynamic working set**: `SimpleContextEngine` (this design).

The comparison is offline and deterministic: the same scripted scenarios are
replayed through all three `ContextEngine` implementations and measured with
the same token estimator (`ascii/4 + non-ascii`, shared by all engines).

## 2. How to run

```bash
# All seven scenarios
cargo run -p agent-replay -- --compare

# One scenario
cargo run -p agent-replay -- --compare long_refactor
```

The same engines are available live in the TUI (the kernel, tools and UI are
unchanged — only the composition-root engine differs):

```bash
cargo run -p agent-tui -- --context=append .
cargo run -p agent-tui -- --context=rolling .
cargo run -p agent-tui -- --context=dynamic .   # default
```

## 3. Metrics

Measured per scenario per engine:

| Metric | Meaning |
| --- | --- |
| `in_tok_total` | Sum of `ContextSnapshot.approx_tokens` over every model round — total model input consumed by the task. |
| `in_tok_max` | Largest single snapshot — the worst single model request. |
| `over_budget` | Snapshots exceeding the configured budget (12 K tokens in the comparison). |
| `churn` | Total lifecycle transitions emitted by maintenance (C: archive/cool/drop; B: collapses). |
| `final_total` / `final_active` | Items retained vs. active at the end of the scenario. |

Scenario scripts mirror the kernel event pattern (user message → maintain →
model rounds with tool results → assistant reply → maintain), so replay
measures the exact same sequence a live run would produce.

## 4. Scenarios

The seven roadmap scenarios live in `crates/agent-replay/src/scenarios.rs`:

| Scenario | Shape | What it tests |
| --- | --- | --- |
| `long_refactor` | 24 turns, 3 changing files, test runs | steady growth of a single long task |
| `test_fix_loop` | 16 test/fix rounds, 34-line logs | repeated large logs in one task |
| `task_switch_and_return` | task A (12) → B (10) → A (6) | explicit task switch and return |
| `superseded_decisions` | 6 superseding decisions + 10 turns | contradictory/superseded design decisions |
| `high_volume_irrelevant_output` | 16 turns of 60-line irrelevant logs | high-volume noise |
| `completed_then_unrelated` | completed task (10) then unrelated task (10) | post-completion contamination |
| `pinned_constraint` | one pin across 15 turns / 3 tasks | pinned constraint survival |

## 5. Current results (2026-08-07, `--compare`, budget 12 K)

Policy C now runs the full P4 policy (decision supersession + error lifecycle
+ entity affinity + explicit dependency graph, all configurable, see §7).

```
scenario: long_refactor - 24-turn async refactor
  engine              in_tok_total   in_tok_max over_budget  churn final_total final_active
  A append-only             626387        17516          22      0         97        97
  B rolling-summary         472298         8853           0     51         47        47
  C dynamic                  83691         1563           0    120         96        24

scenario: test_fix_loop - 16 test/fix rounds
  A append-only             405119        17130          13      0         65        65
  B rolling-summary         306670         8635           0     35         31        31
  C dynamic                  65618         1642           0     80         64        16

scenario: task_switch_and_return - task A -> B -> A
  A append-only              24056          799           0      0         87        87
  B rolling-summary          24056          799           0      0         87        87
  C dynamic                  14032          437           0    128         84         6

scenario: superseded_decisions - superseding design decisions
  A append-only               7636          518           0      0         43        43
  B rolling-summary           7636          518           0      0         43        43
  C dynamic                  9220          569           0     40         42        12

scenario: high_volume_irrelevant_output - 16 turns of irrelevant logs
  A append-only             470368        29319          19      0         49        49
  B rolling-summary         249335         9226           0     35         15        15
  C dynamic                  39067         2326           0     48         48        16

scenario: completed_then_unrelated - completed task then unrelated task
  A append-only             142110         4241           0      0         73        73
  B rolling-summary         142110         4241           0      0         73        73
  C dynamic                  23999          787           0    101         71        10

scenario: pinned_constraint - one pin across three tasks
  A append-only               6265          343           0      0         49        49
  B rolling-summary           6265          343           0      0         49        49
  C dynamic                   5537          243           0     65         46         6
```

## 6. Reading the results

- **Token cost (the headline).** On heavy scenarios C costs **5–12× less**
  model input than A (`84 K` vs `626 K` on `long_refactor`) and never
  exceeds the budget, while A blows past it (13–22 over-budget snapshots).
  B bounds its peak with collapses but still pays ~6× C's cost because every
  retained observation is resent verbatim.
- **Task-switch hygiene.** C's `final_active` collapses to 6 (task A, then
  B) after a switch, and to 10 after a completed task — the old task's
  detail is archived, not resent. A/B keep 73–87 items active (stale
  instructions and completed-task detail leak into every later request).
- **B's trade-off.** Rolling summary bounds *peak* tokens but loses
  history: on `high_volume_irrelevant_output` it keeps only 15 items vs A's
  49 — including, in real use, task-relevant facts that fall outside the
  recency window.
- **P4 improvements.** `superseded_decisions` dropped from 10.7 K to 9.2 K
  input tokens (superseded decisions are archived instead of re-ingested),
  with 3+ explainable `superseded by decision` transitions. Errors now
  persist until verified (`test_fix_loop`/`long_refactor` cost a little more
  than the P3 drop-after-one-turn policy, but failure diagnosis stays
  available across retries and the peak stays far below budget).
- **P4 remainder: entity affinity + dependency graph.** The two new features
  add almost no cost: C's totals are unchanged on six scenarios and +0.9 K
  on `long_refactor` (a few same-file prior items are pulled in as
  dependencies of selected items, each with the explainable reason
  `included as dependency of item <id>`). `superseded_decisions` churn drops
  60 → 40: entity affinity keeps the currently-hot decision stable instead of
  letting it cool between turns. The 1 K-token expansion reserve (carved out
  of the model budget, capped at +8 items per snapshot) is the entire cost of
  traceability.

## 7. Reproducibility

Replay is deterministic: the same scenario events through the same engine
version produce the same metrics. Metrics are token estimates (`ascii/4 +
non-ascii`), not vendor tokenizers; absolute numbers differ across providers,
ratios do not. Scenario content and sizes are fixed in code, not sampled.

Policy C's P4 features are configurable — decision supersession, error
verification, entity affinity and dependency expansion
(`SimpleContextConfig { supersession, error_verification, entity_affinity,
dependency_expansion }`, default on):
`SimpleContextConfig::baseline_v0()` turns all four off and reproduces the
P3-era numbers; `SimpleContextConfig::default()` runs the full P4 policy.

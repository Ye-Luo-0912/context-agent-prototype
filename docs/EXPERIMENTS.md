# A/B/C Context Lifecycle Experiments (P3)

## 1. Purpose

The central hypothesis of this prototype is that a continuously maintained,
task-focused working set (policy C) can beat classic baselines on long coding
tasks. The current experiment is a deterministic, coding-shaped policy replay;
it is not yet the real coding-workload acceptance test:

- **A — append-only**: every message and tool result is resent every model
  turn until the window limit forces a stop.
- **B — rolling marker baseline** (`RollingSummaryEngine`): append like A,
  but when retained history crosses a threshold, drop the oldest part outside
  a verbatim recency window and insert a fixed marker. No model-generated
  compaction is run, so B must not be presented as a competitive summarizer.
- **C — dynamic working set**: `SimpleContextEngine` (this design).

The comparison is offline and deterministic: the same scripted scenarios are
replayed through all three `ContextEngine` implementations and measured with
the same token estimator (`ascii/4 + non-ascii`, shared by all engines).

This replay is one half of the evaluation story: the deterministic coding
fixtures that drive the real builtin tool surface (four arms, hidden
verification, event-derived cost accounting) live in `agent-eval` (`--fixtures`,
`--fixture <id>`, `--compare-arm <id>`, `--compare-live <id>`,
`--evidence-dir`, `--show-evidence`, `--preregister`, `--analyze-evidence`, `--metrics`; see `docs/ROADMAP.md` M15).
The live paired coding path is now runnable and writes versioned cell
bundles; it does not by itself close the 300×3 non-inferiority gate.

## 2. How to run

```bash
# All seven scenarios
cargo run -p agent-replay -- --compare

# One scenario
cargo run -p agent-replay -- --compare long_refactor

# Completion-quality proxy: same comparison plus key-fact coverage
cargo run -p agent-replay -- --facts
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
| `in_tok_total` | Sum of the replay's fixed system estimate + `MaterializedContext.approx_tokens`; a context-policy proxy, not total provider input. |
| `in_tok_max` | Largest replay estimate, not the complete assembled provider request. |
| `over_budget` | Model inputs exceeding the configured budget (12 K tokens in the comparison). |
| `churn` | Maintenance transitions only; it does not include all GC/store work. |
| `final_total` / `final_active` | Engine diagnostics at the end. Since CTX-09 (2026-08-12) `total_items` is the full logical catalog (resident heap + warm buffer + external store). The 2026-08-07 tables below predate that change: their `final_total` column is the older Resident-only count. |

The `--facts` mode adds the completion-quality proxy (`crates/agent-replay/src/facts.rs`):
each scenario declares key facts — content needles that must be in the
model-visible working set during a turn window (required: previous failure,
final decision, current file) or must not (forbidden: superseded decision,
completed task's detail). Every miss is explainable as "fact X was not in
view on turn N".

| Fact metric | Meaning |
| --- | --- |
| `req_met` / `req_viol` | Window turns in which required facts were in view / out of view. |
| `forb_viol` | Forbidden facts that leaked into view at least once (stale-instruction leakage). |
| `coverage` | `req_met / req_viol + req_met` — required-fact coverage ratio. |

Scenario scripts mirror the main context event pattern (user message →
maintain → model rounds with tool results → assistant reply → maintain). They
exercise the same engines, but do not reproduce the full runtime request,
PromptAssembler roles, tool-surface cost, provider behavior or repository
outcome.

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

- **Estimated context cost (the headline).** On heavy scenarios C costs
  **5–12× less** under this replay estimator than A (`84 K` vs `626 K` on
  `long_refactor`) and never
  exceeds the budget, while A blows past it (13–22 over-budget snapshots).
  B bounds its peak with collapses but still pays ~6× C's cost because every
  retained observation is resent verbatim.
- **Task-switch hygiene.** C's `final_active` collapses to 6 (task A, then
  B) after a switch, and to 10 after a completed task — the old task's
  detail is archived, not resent. A/B keep 73–87 items active (stale
  instructions and completed-task detail leak into every later request).
- **B's trade-off.** The rolling-marker baseline bounds *peak* tokens but loses
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

## 7. Key-fact coverage (completion-quality proxy, 2026-08-10, `--facts`, budget 12 K)

These numbers were regenerated after fixing the evaluator to run cost and
fact coverage on independent fresh engine instances. The earlier code replayed
the scenario twice into one engine before measuring coverage; the corrected
results below happen to retain the same table values, but they no longer rely
on contaminated state. A regression compares each aggregate coverage result
with a standalone fresh replay.

```
scenario: task_switch_and_return - task A -> B -> A
  engine              in_tok_total    req_met   req_viol forb_viol coverage
  A append-only              22476        0/0          0         1  100.0%
  B rolling-summary          22476        0/0          0         1  100.0%
  C dynamic                   6708        0/0          0         0  100.0%
  fact [forbidden] task B's middleware detail must not contaminate task A's finish:
      A append-only: VIOLATED (first: turn 23);  B rolling-summary: VIOLATED;  C dynamic: ok

scenario: superseded_decisions - superseding design decisions
  A append-only               7054        9/9          0         1  100.0%
  B rolling-summary           7054        9/9          0         1  100.0%
  C dynamic                   4469        9/9          0         0  100.0%
  fact [must-see] the final decision stays in view through implementation: all ok
  fact [forbidden] the superseded first decision must not contaminate implementation:
      A append-only: VIOLATED;  B rolling-summary: VIOLATED;  C dynamic: ok

scenario: completed_then_unrelated - completed task then unrelated task
  A append-only             140365        9/9          0         1  100.0%
  B rolling-summary         140365        9/9          0         1  100.0%
  C dynamic                  16949        9/9          0         0  100.0%
  fact [must-see] the CSV task's file stays in view: all ok
  fact [forbidden] the completed pagination detail must not contaminate the CSV task:
      A append-only: VIOLATED;  B rolling-summary: VIOLATED;  C dynamic: ok

scenario: test_fix_loop - 16 test/fix rounds
  A append-only             403352      15/15          0         0  100.0%
  B rolling-summary         305245      15/15          0         0  100.0%
  C dynamic                  56430      15/15          0         0  100.0%
  fact [must-see] every fix round sees the previous failure: all ok

scenario: pinned_constraint - one pin across three tasks
  all engines 15/15 required turns, coverage 100%
```

Reading:

- **Stale-instruction leakage is measured, and C is clean.** On all three
  contamination scenarios the forbidden fact (task B's detail in task A's
  finish, the superseded first decision, the completed task's pagination
  code) leaks into A's and B's model-visible working set and never leaks in
  C — while C pays 3–9× less than A for the same required-fact coverage.
- **Required facts hold in C.** The previous failure is visible in every
  fix round (15/15), the final decision through implementation (9/9), the
  active task's file (9/9) and the pinned constraint in every turn.
- **Honest negative: `long_refactor`.** C loses the previous step's file
  content on 2 of 4 window turns (the final fix still sees the previous
  failure — the core success premise holds). A/B keep it. In a long
  refactor that cycles files, the most recent content of the non-current
  file leaves C's working set too early — a real incorrect-eviction signal
  and the documented input for the next non-vector policy iteration, not a
  hidden pass.

## 8. Reproducibility

Replay is deterministic: the same scenario events through the same engine
version produce the same metrics. Metrics are token estimates (`ascii/4 +
non-ascii`), not vendor tokenizers; absolute numbers and ratios may differ once
provider tokenization and fixed request layers are counted. Scenario content
and sizes are fixed in code, not sampled.
Every outcome/coverage observation starts with its own new engine.

Policy C's P4 features are configurable — decision supersession, error
verification, entity affinity and dependency expansion
(`SimpleContextConfig { supersession, error_verification, entity_affinity,
dependency_expansion }`, default on):
`SimpleContextConfig::baseline_v0()` turns all four off and reproduces the
P3-era numbers; `SimpleContextConfig::default()` runs the full P4 policy.

## 9. M15 acceptance still required

Before claiming real-evaluation completion:

- replace or supplement B with actual bounded compaction and count its model
  cost;
- run paired feature/bug/refactor tasks with real repository tools and hidden
  build/test verification;
- count provider I/O, PromptAssembler/TurnFrame/tool schemas, context and
  compactor/recall/store work, tool execution and wall time;
- report repeated runs and a predeclared non-inferiority bound for task
  success, plus required-fact recall, stale/terminal leakage and recovery
  faults;
- retain logical-catalog counts, Resident bytes, candidate counts and
  materialization latency so bounded prompt size cannot hide growing GC work.
  These measurements are landed: `agent-eval` aggregates catalog counts,
  selected items/tokens, store I/O and materialize p50/p95; retrieval metrics
  cover search recall/latency, found-after-forgotten and graded-access stamps;
  replay reports Resident final/preview-peak bytes. The remaining work is to
  make every live cell durable and auditable, not to add another aggregate.
  Decision (2026-08-14): Search/GC evaluation is folded into these same
  cells — retrieval metrics are secondary lifecycle endpoints of the
  paired A/B/C runs, recorded per cell, not a separate later experiment.

### Contemporaneous smoke bound (2026-08-14; not a formal preregistration)

This bound was written before the first real-model coding cells, but no signed
or committed suite/analysis manifest predates those cells. Treat it as the
historical smoke plan, not independently auditable preregistration evidence.

- **Tasks.** The four one-turn hidden-verification fixtures in `agent-eval`
  (`fix_off_by_one`, `implement_stub`, `rename_symbol`, `add_test`).
  Adding or rewriting a fixture after seeing live failures does **not**
  change this bound; new cells are logged separately below.
- **Pairing.** Append-only / rolling-summary / dynamic, each on a fresh
  seeded workspace. Same builtin tool surface. Live cells use a real
  tool-capable provider; B's rolling fold still uses the scripted digest
  (not a model summarizer).
- **Success.** Hidden file-content `verify`, not model self-report.
- **Harness smoke (not the gate).** `--compare-live <id>` (one fixture ×
  three engines) and `--compare-live-all` (every coding fixture). A 3-repeat
  loop (`--repeats N --compare-live <id>`) only validates variance of the
  harness.
- **Proposed gate (still open).** Minimum 30 tasks × 3 repeats. Success
  non-inferiority: 95% interval lower bound on (C − A) success-rate
  difference no worse than −5 percentage points. The formal run must first
  freeze the task-clustered estimator, power and failure rules described
  below; repeats are not independent tasks. Both-pass token comparison is a
  secondary mechanism diagnostic, not a replacement for intent-to-treat cost.
- **Do not claim.** This smoke, `--compare-arm` (scripted), or the
  no-tool retention live run closes M15.

### Live paired smoke (2026-08-14)

`--compare-live-all` against pinaic `gpt-5.6-luna`, one repeat, independent
workspaces, hidden file-content verify (12 cells, ~4 min wall):

| fixture | append | rolling | dynamic |
| --- | --- | --- | --- |
| fix_off_by_one | pass (23713 in) | pass (25433) | pass (41895) |
| implement_stub | pass (23586) | pass (17370) | pass (36578) |
| rename_symbol | pass (23647) | pass (23719) | pass (23688) |
| add_test | fail (29465) | fail (30364) | fail (29697) |

Success 3/4 on every engine (C − A = 0 on n=4). `add_test` failed on all
three: hidden verify only accepts an edit inside `src/calc.py`
(`def test_add` or `assert add(`); a real model can “pass” the prose
task by writing a different file. That is a fixture-spec gap, not an
engine split.

Live cells are **not** a controlled materialization-only token comparison:
the model chose a different tool trace per engine (dynamic often more rounds).
That difference is nevertheless valid end-to-end cost and must be retained in
the formal intent-to-treat result. `--compare-arm` isolates the same-script
mechanism; B still folds with the scripted digest. This validates the live
harness. It does not close the 300×3 gate.

### What this shows (2026-08-14)

**Success.** On these four tiny coding fixtures, dynamic did not lower
hidden-check success versus append-only: both 3/4, C − A = 0. The one
shared miss (`add_test`) is the same on A/B/C, so it is not a working-set
regression.

**Tokens.** Two different measurements, do not mix them:

- Same script, many turns (`--compare-arm`, CI): dynamic feeds the model
  fewer input tokens than append (e.g. `fix_off_by_one` 11 862 vs
  12 849). The gap is real but a bounded fraction of the total because
  tool schemas and the system prompt dominate each round. Replay-scale
  traces still show large savings on long workloads.
- Real model, one task prompt (`--compare-live-all`): dynamic often
  *spent more* because it took more tool rounds. The smoke is too small for a
  cost claim, but the extra rounds are a real efficiency signal to explain.

**M15.** The live paired *harness* works (tools, dotted-name mapping,
independent workspaces, hidden verify). The milestone is **not** closed:
n=4 × 1 repeat cannot support the historical −5 pp / 30×3 proposal,
nor the EVAL-01.3 300×3 amendment;
a model-backed B, the 300-task freeze and Phase 4 remain open. Live cells
now write versioned evidence bundles (EVAL-01.1); `--preregister` freezes
the C−A clustered estimator (EVAL-01.2) and the 300×3 sample size
(EVAL-01.3). Historical 30×3 is underpowered. The 2026-08-14 smoke itself still cannot be rebuilt because
those workspaces were deleted.

### More reasonable live cells (registered 2026-08-14, before re-run)

The first live smoke showed two fixture problems, not an engine split:

1. **`add_test` prompt vs hidden check.** Verify still only accepts an
   edit inside `src/calc.py`. The model-visible prompt now names that
   file and forbids a new test file. This pins the *description* to the
   existing `expected_edit`; it does **not** widen the hidden check after
   seeing failures.
2. **One-turn tiny edits do not exercise working-set policy.** New
   fixture `recall_after_fix`: fix `src/util.py`, three unrelated
   `src/scratch.md` notes, then create `src/main.py` that must call the
   already-fixed `visit_all` and must not reintroduce `i + 1`. Live
   sends five user turns (`live_turns`). Scripted `--compare-arm` for
   this id uses one tool per turn; the original four keep the packed
   first-turn tool-loop so the ≥300 token CI floor does not move.

`--compare-live-reasonable` runs `add_test` then `recall_after_fix`.
These cells are diagnostics, not a rewrite of the n=4 bound above.
Do not claim M15 closed from them.

### Reasonable live re-run (2026-08-14)

`--compare-live-reasonable` against pinaic `gpt-5.6-luna`, one repeat,
independent workspaces, hidden file-content verify (~4 min wall):

| fixture | append | rolling | dynamic |
| --- | --- | --- | --- |
| add_test | pass (17498 in, 1 turn, 2 tools) | pass (17611, 1t, 3 tools) | pass (24057, 1t, 5 tools) |
| recall_after_fix | pass (82257, 5t, 9 tools) | pass (89952, 5t, 12 tools, manager 26) | pass (124223, 5t, 23 tools, lifecycle 66) |

Success 2/2 on every engine (C − A = 0 on these two diagnostics).
Pinning the `add_test` prompt to `src/calc.py` flipped the earlier
shared miss without changing the hidden check. `recall_after_fix` is
the first live cell that actually occupies context with unrelated
notes before asking the model to reuse the fix; all three engines
kept `util.py` fixed and wrote a `main.py` that calls `visit_all`
without `i + 1`. Dynamic searched once (empty) and recorded
recovered/forgotten items — the working set moved — and still passed.

Live tokens remain **not** a controlled same-trace comparison: the model chose
a different trace per engine (dynamic 20 rounds vs append 13 on recall).
Scripted `--compare-arm` isolates same-trace context cost, while the live total
is the end-to-end efficiency signal. B still folds with the scripted digest.
n=1 repeat on two diagnostics does not close the 300×3 / −5 pp gate.

### Persisted reasonable retry (2026-08-14)

`--compare-live-reasonable` against pinaic `gpt-5.6-luna` after a region
503, one repeat, EVAL-01.1 bundles in
`target/eval-evidence/reasonable-live-retry` (~4.5 min wall). Arm order
was rolling → dynamic → append. Usage complete, seq contiguous, lag 0.

| fixture | append | rolling | dynamic |
| --- | --- | --- | --- |
| add_test | pass (17516 in, 3 rounds, 2 tools) | pass (17519, 3r, 2 tools) | pass (33321, 5r, 5 tools) |
| recall_after_fix | pass (89470, 14r, 11 tools) | **verify_failed** (72591, 14r, 11 tools) | pass (102193, 17r, 21 tools) |

Hidden-check C − A = 0 on both fixtures. Rolling missed `src/main.py` on
the last `recall_after_fix` turn (no tools that turn). ITT
`--analyze-evidence` is ineligible (n=2, repeats=1, suite not frozen).

**Why C spent more (from events.jsonl, not from the deleted first smoke).**
Not empty search this time (`search_calls=0`). The extra cost is a
treatment effect:

1. `recall_after_fix` turn 1 on C emitted no tools (empty assistant) and
   deferred the util fix until the HDMI-append turn.
2. C then mixed leftover repair with the current note and a verification
   loop: `git.status` ×3 (exit 128, temp workspace is not a git repo),
   `shell.exec` ×3 failed (`from src.util import` / `printf | python`)
   then one successful `cd src && python main.py`. A did one failed
   `git.status` and never shelled.
3. C reread more (`repeated_fs_reads` 4 vs 2) and listed `src` twice.
4. `add_test` C also paid two extra failed probes after a correct edit
   (`git.diff` exit 129, `python -m pytest src/calc.py` exit 1).

Do not retune scoring from n=1. Do not treat C's extra rounds as unfair
noise. B is still a scripted digest. This does not close M15.

### What the reasonable cells show (2026-08-14)

**Success.** After the fixture-spec gap is closed, dynamic did not
lower hidden-check success versus append-only on either the pinned
one-turn `add_test` or the five-turn `recall_after_fix`. Both engines
2/2, C − A = 0. The earlier shared miss was underspecified prose, not
a working-set regression. On the first live cell that actually fills
context with unrelated notes and then reuses a fix, C still passed
while its working set moved (lifecycle, one empty search,
recovered/forgotten counts).

**Tokens.** Keep both estimands explicit: identical-script `--compare-arm`
isolates materialization cost, where C costs less input than A; live pairing
measures end-to-end behavior, where C often spends more because it takes more
tool rounds. Do not substitute one for the other. Short live tasks still do
not demonstrate the large replay-scale savings.

**M15.** These diagnostics make the live gate *honest* (prompt matches
verify; at least one multi-turn recall analogue exists). They do **not**
close the milestone: n is tiny, repeats = 1, B is not a model
summarizer, and the 300×3 / −5 pp gate has not been executed.
EVAL-01.1 persists the next live cells; EVAL-01.2 freezes the estimator;
EVAL-01.3 re-freezes n=300 / repeats=3 (historical 30×3 is underpowered).
Neither reconstructs the 2026-08-14
`recall_after_fix` traces. Next evidence that would change the claim is a
frozen 300-task suite plus a larger paired coding set, not another
one-line edit.

### Historical Resident diagnostic before replay parity (2026-08-14)

Instrumentation: `ContextDiagnostics.resident_bytes` (Resident heap UTF-8
bodies), `RunMetrics` final + peak, `agent-replay --compare` columns
`res_bytes` / `peak_bytes`. The 10,000-turn episode fixture now asserts
byte flatten as well as item count. This was the last Phase 0 measurement
named before changing scoring weights.

The following table is retained as the observation that found a harness gap;
it is **not** the current policy result. Synthetic replay had omitted the
actor's turn-boundary `ContextGc`, so it measured archived attention before
residency movement ran.

**What that pre-fix run appeared to split:**

| Workload | C vs A prompt tokens | C vs A Resident bytes |
| --- | --- | --- |
| `long_refactor` replay | 81 372 vs 622 560 (~8× cheaper) | 70 572 vs 69 332 (**≈ A, slightly more**) |
| `test_fix_loop` | 61 350 vs 403 352 | 68 780 vs 67 960 (**≈ A**) |
| `high_volume_irrelevant_output` | 250 503 vs 469 568 (~2×) | 117 254 vs 116 866 (**≈ A**); **B** 36 587 (drops history) |
| scripted `fix_off_by_one` | 12 744 vs 13 761 | 492 vs 1 056 (C smaller — short task) |
| scripted `recall_after_fix` | 16 438 vs 18 078 | 649 vs 1 173 (C smaller — short task) |

C's `final_active` on `long_refactor` is 24 vs A's 97, so **attention**
archives. The bodies stay on the Resident heap. A smaller prompt was
hiding an unbounded-looking catalog. The 10k synthetic dialogue still
flattens both counts and bytes (episode rotation); the coding replay
does not move archived bodies off Resident.

`long_refactor --facts` on this run: C required-fact coverage 4/4, no
forbidden leak. Do not treat the older ROADMAP “2 of 4 window misses”
as current without a new failing fact.

**The direction inferred at that point (superseded by the parity run below):**

1. **Residency movement, not selection scoring.** Archived / irrelevant
   bodies should go Warm → Cold → External (reversible, recallable).
   Retuning `SimpleContextConfig` scores will not shrink the heap.
   Rolling B “wins” bytes on `high_volume` by destroying history; C
   must externalize, not drop.
2. **Do not use short coding fixtures to accept heap policy.** They
   already show a smaller C heap because the run is tiny. Re-measure
   `long_refactor` / `high_volume` after any residency change.
3. **M15 300×3 remains a different question** (task success
   non-inferiority with a real model; historical 30×3 was underpowered).
   It does not replace (1). Do not
   start PLAT-05+ or a model-backed B to answer the heap split.

M15 remained open. The required re-measurement is the parity run below.

### Replay now drives turn-boundary full GC (2026-08-14)

The previous Resident-bytes table was taken **without** `ContextGc` in
the synthetic scenarios. The live `RuntimeActor` runs full GC after
AfterModel maintain and before `TurnCompleted`. Replay only called
`engine.gc()` on `ContextGc` events, so C archived attention and left
every body on the Resident heap — the “heap ≈ A” reading was a harness
gap, not proof that eviction policy is missing.

After inserting `ContextGc` in `Script::turn` (and post-completion
compact in `done()`), same scenarios, same engines, same 12 K budget:

| Workload | C vs A prompt tokens | C vs A Resident bytes (final / peak) |
| --- | --- | --- |
| `long_refactor` | 70 104 vs 622 560 | **4 298 / 7 238** vs 69 332 / 69 330 |
| `test_fix_loop` | 58 398 vs 403 352 | **1 029 / 9 433** vs 67 960 / 67 958 |
| `high_volume_irrelevant_output` | 128 228 vs 469 568 | **22 367 / 29 664** vs 116 866 / 116 864 (B 36 587 / 36 615) |
| `completed_then_unrelated` | 18 019 vs 140 365 | **528 / 3 692** vs 16 456 / 16 454 |

C's Resident heap is now a small fraction of A's. On `high_volume`, C
also undercuts B's bytes without destroying history (bodies go Warm /
stay recallable). CI asserts final and peak Resident bytes below half
of append-only on the three heavy scenarios.

**Fact coverage (then).** Required/forbidden facts held on
`test_fix_loop`, task-switch, supersession, completed-then-unrelated,
and the pin. `long_refactor` C was **3/4** required: `fn handle_21()`
missing on turn 23.

### Latest file body of the active task (2026-08-14)

Cause: successful `fs.read`-shaped observations are ephemeral. AfterModel
archived them and turn-boundary GC evicted them the same turn. The next
user message named a *different* file, so hot-entity reactivation did not
bring the previous path back. Scoring was not the lever.

Policy (explicit, bounded, scoring frozen): keep the latest successful
observation whose first line is a file path, per path, for the **active
task only**, cap 8 paths. A newer read of the same path supersedes the
old body (semantic death). Task switch / completion drops the roots.
Build logs that merely mention a file still consume-and-evict.

Re-measured, same 12 K budget, turn-boundary GC still on:

| Workload | C vs A prompt tokens | C vs A Resident bytes (final / peak) |
| --- | --- | --- |
| `long_refactor` | 71 567 vs 622 560 | **4 298 / 7 238** vs 69 332 / 69 330 |
| `test_fix_loop` | 56 810 vs 403 352 | **827 / 9 232** vs 67 960 / 67 958 |
| `high_volume_irrelevant_output` | 128 228 vs 469 568 | **22 367 / 29 664** vs 116 866 / 116 864 (B 36 587 / 36 615) |
| `completed_then_unrelated` | 17 031 vs 140 365 | **376 / 3 546** vs 16 456 / 16 454 |

`long_refactor` C required facts are **4/4** (`fn handle_21()` on turns
22–24). Forbidden facts still 0 on C (task-switch / supersession /
completed-then-unrelated). Prompt tokens on `long_refactor` rose ~1.5 K
vs the 3/4 run because three small file bodies stay in view; Resident
bytes did not grow. Do not treat that token bump as a reason to skip GC.

### Route decision after the M15 diagnostic (2026-08-14)

The parity run changes the immediate context route: basic reversible residency
movement is present and effective, so do **not** reopen the broad
"externalize archived bodies" implementation as if it were absent. The
current-file / `handle_21` slice is landed (active-task latest file body,
4/4 on `long_refactor`, heap still a fraction of A). What remains:

1. inspect the live A/C traces that made C take more model/tool rounds.
   **Partial 2026-08-14.** `target/eval-evidence/reasonable-live-retry`
   shows C's extra `recall_after_fix` rounds were a no-tool first turn
   plus failed git/shell probes and extra rereads, not empty search.
   The same-day `reasonable-live` attempt was all ITT 503/region errors.
   n=1; do not retune policy;
2. before the formal gate, make each intended cell produce a versioned result
   bundle. **EVAL-01.1 (2026-08-14) landed the writer:** live `--compare-live*`
   writes `agent-eval.cell.v1` (manifest, events.jsonl, summary, workspace
   hash, verify.json) and `--show-evidence` rebuilds the table.
   **EVAL-01.1b (2026-08-14):** `verify.json` records named file-content
   asserts plus bounded bodies so the hidden check can be replayed after the
   workspace is deleted. Scoring of the five smoke fixtures is unchanged;
   this is not pytest/build. A persisted `recall_after_fix` pair now
   exists under `target/eval-evidence/reasonable-live-retry`;
3. freeze a heterogeneous suite of at least 300 independent bug/feature/
   refactor/long-tool-loop tasks with executable hidden build/tests. Three
   repeats estimate within-task variance; they are not 900 independent tasks.
   **EVAL-01.2 (2026-08-14) froze the estimator, not the suite.**
   **EVAL-01.3 (2026-08-14) re-froze n/repeats:** 300 tasks × 3 repeats,
   margin still −5 pp. Under the conservative model (A ⟂ C | task, seed
   20260814, 5000 sims) historical 30×3 has P(pass|Δ=0)=961/5000 ≈ 19%;
   300×3 has 4048/5000 ≈ 81%. Do not collect acceptance cells until the
   suite is frozen. Do not invent 300 one-line fixtures. Current FIXTURES
   remain 5 smoke/diagnostic tasks. EVAL-01.3b freezes the suite
   (`suite_frozen=true`, pack 509=9 file + 500 SWE-bench Verified) and
   declares retrieval secondaries; n/repeats/margin stay 300×3 / −5 pp.
   Do not collect 300×3 acceptance cells until the frozen ~30×3
   calibration pilot. Unit tests do not pull images.
   **EVAL-01.5 (2026-08-14):** 30-id calibration sample frozen
   (`--pilot`, sha256 `fa8c5308…`, 10/10/10). `--pilot-run` is the live
   A/B/C path on that sample (default 9 file tasks; SWE-bench is
   `--include-swebench` + clone/Docker opt-in). `--pilot-calibrate`
   reports `decision=pilot` and cannot pass the 300×3 gate.
   File-only live 9×3 collected in `crates/agent-eval/evidence/pilot-30`
   (81 cells; ITT A=C=0.778; diagnostic C−A LCL=−0.146; gate
   ineligible). 21 SWE-bench cells not collected. Do not amend n.
   Live `--compare-live*`
   now counterbalances append/rolling/dynamic per fixture×repeat;
4. use a model-backed bounded compactor for B and include its manager cost.

Until those artifacts and the predeclared interval exist, the live tables are
diagnostic observations rather than independently reproducible acceptance
evidence. M15, V2, learned/vector policy and transport-selection gates remain
closed; M12/M13 continue on their independent trusted-execution route.

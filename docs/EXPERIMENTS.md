# A/B/C Context Lifecycle Experiments (P3)

## 1. Purpose

The central hypothesis of this prototype is that a continuously maintained,
task-focused working set (policy C) can beat classic baselines on long coding
tasks. The current experiment is a deterministic, coding-shaped policy replay;
it is **not** the M15 decision instrument. That instrument is the Context
Benchmark (`agent-eval --context-bench`, pack
`crates/agent-eval/context-bench/`): 12 live coding tasks that ask where
dynamic context helps or hurts. Replay still proves policy on scripted
events (`long_refactor`, `superseded_decisions`, `task_switch_and_return`,
`high_volume_irrelevant_output`). The frozen 300×3 ITT gate stays parked.

- **A — append-only**: every message and tool result is resent every model
  turn until the *send* window forces a trim. Live P1 does not starve A
  with a smaller send limit than C; A is a competent long-context baseline.
- **B — rolling marker baseline** (`RollingSummaryEngine`): append like A,
  but when retained history crosses a threshold, drop the oldest part outside
  a verbatim recency window and insert a bounded summary. Live P1 injects the
  shared `ModelBackedCompactor` (same operator as C's `TaskCompleted`
  distill); CI keeps a scripted digest. Compactor provider tokens are
  counted separately from the visible working set. B packs against the
  same kernel working-set cap as C.
- **C — dynamic working set**: `SimpleContextEngine` (this design). Live P1
  packs C against the kernel ~24k working-set budget even when the provider
  send window is 128k. Extra tool rounds under the shared 48-round cap are a
  treatment effect, not a reason to give C more rounds than A.

The comparison is offline and deterministic: the same scripted scenarios are
replayed through all three `ContextEngine` implementations and measured with
the same token estimator (`ascii/4 + non-ascii`, shared by all engines).

This replay is one half of the evaluation story: the deterministic coding
fixtures that drive the real builtin tool surface live in `agent-eval`
(`--fixtures`, `--compare-live`, `--context-bench`, `--preregister`; see
`docs/ROADMAP.md` M15). Replay ≠ live Context Bench. Neither closes M15.

## 2. How to run

```bash
# All seven scenarios
cargo run -p agent-replay -- --compare

# One scenario
cargo run -p agent-replay -- --compare long_refactor

# Completion-quality proxy: same comparison plus key-fact coverage
cargo run -p agent-replay -- --facts

# Context Bench pack + seed/golden/hidden-command self-check (no model).
# --context-bench-run is live A/C (27 cells). Python must be present.
cargo run -p agent-eval -- --context-bench
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

- live B now uses a shared model-backed bounded compactor (CI keeps the
  scripted digest) and counts its provider tokens; episode-rotation
  distillation and executable hidden build/tests remain open;
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
  `--analyze-evidence` rolls the same secondaries up from cell bundles
  (search recall/latency, found-after-forgotten, graded-access stamps)
  on cost-eligible A/C pairs; they are not in the primary LCL. The
  engine-only `--retrieval` dashboard remains the catalog baseline.

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
a P1 SWE-bench cohort, the 300-task freeze and Phase 4 remain open. Live cells
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
   EVAL-01.3c locks the exact 300 acceptance ids (sha256 `7ff6b5dd…`,
   harvest sizes 107/147/46) so the 509 pack is not an optional pool;
   cost-eligible paired tokens omit usage-incomplete zeros; pooled φ is
   not the A ⟂ C | task diagnostic. Do not collect 300×3 acceptance
   cells until remaining calibration. Unit tests do not pull images.
   **EVAL-01.5 (2026-08-14):** 30-id calibration sample frozen
   (`--pilot`, sha256 `fa8c5308…`, 10/10/10). `--pilot-run` is the live
   A/B/C path on that sample (default 9 file tasks; SWE-bench is
   `--include-swebench` + clone/Docker opt-in). `--pilot-calibrate`
   reports `decision=pilot` and cannot pass the 300×3 gate.
   File-only live 9×3 collected in `crates/agent-eval/evidence/pilot-30`
   (81 cells; ITT A=C=0.778; diagnostic C−A LCL=−0.146; gate
   ineligible). **EVAL-01.5.p1 (2026-08-15):** P0 host was kernel-fallback
   send 19904 + 12 rounds (and kernel `max_tool_rounds` 16). That cannot
   host SWE-bench: empty context still overflowed the send floor, and the
   round cap aborted tool loops. Remaining P0 SWE-bench spend is skipped;
   do not mix P0 and P1 cells in one ITT table; do not re-run file-only
   under P1 for the same table. P1 shared host: declared send window
   (eval default 128000, `OPENAI_CONTEXT_WINDOW` override), output reserve
   4096, 48 rounds for A/B/C (harness + kernel). Engine treatment: C/B
   pack to kernel 24k; A grows until the send window. Do not give A a
   smaller send than C or C a higher round cap than A. Do not amend n.
   Live `--compare-live*`
   now counterbalances append/rolling/dynamic per fixture×repeat;
4. **Partial 2026-08-15:** live B/C use a model-backed bounded
   compactor; CI keeps the scripted digest; manager/compactor cost is
   counted. Episode-rotation distillation and executable hidden
   build/tests remain open.
   **EVAL-01.5.p1c (2026-08-15):** C extra-round diagnosis stands: not
   missing current-turn tool output. Causes were prompt distrust (working
   set called a cache / optional), empty search of a still-Resident file,
   a no-tool first turn, failed probes, and extra rereads. Catalog search
   is catalog-wide. System prompt and assembler headers are labels/facts,
   not retrieval tutorials. Extra rounds remain a treatment effect to
   re-measure. Smoke fixtures stay file-content; executable hidden stays
   on the suite pack. **P1 n=1 rehydration diag (2026-08-15):** 9
   file-only + recall in `target/eval-evidence/rehydration-diag`. pep616
   C−A 19/27→15/20 vs 13/16; js-ms-minutes C extra tools gone; recall
   extra rounds remain (21/25 vs A 15/12). Mixed leftover on the rest
   (js-ms-negative C 14r vs A 5r; rust-jcs C fewer). Empty-assistant
   flake on js-ms-minutes B, openai-wire B, rust-grep C. Cell compact
   harvest now sums `ContextMaintained` pass costs (old cells still
   show `compact=0/0`). **P1 SWE-bench n=1 (2026-08-15):** 3 Django
   tasks in `target/eval-evidence/p1-swebench-diag` (pre-path-stamp).
   C 3/3 pass; A 0/3 at 48-round cap; B mixed (11749 empty-assistant
   flake). C search 0. **P1 after-path n=1 (2026-08-15):**
   `target/eval-evidence/p1-after-path`. js-ms-negative C extra rounds
   gone (9r vs A 11r, C search 1/2). recall C still 21r/30t search 0.
   **P1 after-anchor n=1 (2026-08-15):** `p1-after-anchor`. recall A/B/C
   verify-failed (`4B`); C 16r vs A 13r, search 0. js-ms-negative C 10r
   pass ≈ A 11r, search 0; B hidden fail. `--analyze-evidence` on that
   dir: search A/C calls=0/0; C forgotten/recovered 15.5/2.5 vs A 0/0.
   **P1 SWE-bench after-anchor n=1 attempt (2026-08-15):**
   `p1-swebench-after-anchor` / `django-13344` A/B/C HTTP 403 on turn 1
   (`outcome=error`, cost-missing 1.0). Direct connect; WinINET proxy
   `127.0.0.1:7897` was on, but reqwest lacked `system-proxy`. curl via
   that port is 200. Infrastructure, not a coding split; not mixed into
   P0 or `p1-swebench-diag`.
   **P1 SWE-bench after-proxy n=1 (2026-08-15):** `p1-swebench-after-proxy`
   / `django-13344` A/B/C `verify_failed` (gold unresolved), not 403.
   C 33r/59t 926950in search 2/3 forgotten 48 recovered 0; A 47r/69t
   1550204in search 0. Ineligible n=1. Not mixed into P0, the 403 dir,
   or `p1-swebench-diag`.
   **P1 after-proxy `django-13809` (2026-08-15):** B gold passed; A/C
   48-round cap. `--analyze-evidence` n=2 ineligible; ITT A=C=0;
   tokens A 1573556 / C 1076972. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `django-14007` (2026-08-15):** A 48-round cap; B/C
   `verify_failed`. `--analyze-evidence` n=3 ineligible; ITT A=C=0;
   tokens A 1477619.7 / C 1141977.3. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `django-14011` (2026-08-15):** A/B/C `verify_failed`
   (C 27r `681903`; A 29r `594658`). `--analyze-evidence` n=4 ineligible;
   ITT A=C=0; tokens A 1256879.2 / C 1026958.8. Not mixed into P0 / 403
   / diag.
   **P1 after-proxy `django-15268` (2026-08-15):** B gold passed; C
   48-round cap; A HTTP 401 `INVALID_API_KEY` (`usage_incomplete`).
   `--analyze-evidence` n=5 ineligible; cost-missing 1/5. Operator: 401 is
   relay jitter; continue same dir. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `django-15503` (2026-08-15):** A `verify_failed` 20r
   `493440`; B/C 48-round cap (C search 1 empty). `--analyze-evidence`
   n=6 ineligible; ITT A=C=0; tokens A 1104191.4 / C 1062320.8. Not mixed
   into P0 / 403 / diag.
   **P1 after-proxy `django-15695` (2026-08-15):** A/B/C `verify_failed`
   (C 44r `1503592` search 1/1 forgotten 55 recovered 0).
   `--analyze-evidence` n=7 ineligible; ITT A=C=0; tokens A 1197934.7 /
   C 1135866.0. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `django-16642` (2026-08-15):** A/B/C `verify_failed`
   (C 8r `77182` search 1 empty forgotten 14 recovered 0).
   `--analyze-evidence` n=8 ineligible; ITT A=C=0; tokens A 1102014.4 /
   C 984625.4. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `matplotlib-23314` (2026-08-15):** A/B/C gold passed
   (C 35r `751527` search 2/14 forgotten 43 recovered 0). First both-pass
   pair (tokens C-A=-21745). `--analyze-evidence` n=9 ineligible; ITT
   A=C=0; tokens A 1060921.6 / C 955488.1. Not mixed into P0 / 403 /
   diag.
   **P1 after-proxy `pylint-4551` (2026-08-15):** A/B turn-4 48-round
   cap; C `verify_failed` 63r forgotten 99 recovered 12. First non-zero
   recover in this dir. `--analyze-evidence` n=10 ineligible; ITT A=C=0;
   tokens A 1428904.0 / C 1026865.2. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `pytest-5787` (2026-08-15):** A/B/C `verify_failed`
   (C 31r `863738` search 1/2 forgotten 30 recovered 0).
   `--analyze-evidence` n=11 ineligible; ITT A=C=0; tokens A 1400711.7 /
   C 1010552.5. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `pytest-6202` (2026-08-15):** A/B gold passed; C
   48-round cap. First A=1 C=0 pair. `--analyze-evidence` n=12
   ineligible; ITT mean=-0.083 LCL=-0.233 `degenerate=false`; tokens A
   1327469.2 / C 1018846.4. Not a gate. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `pytest-7571` (2026-08-15):** A/B/C gold passed (C
   38r `756640` forgotten 47 recovered 0). Second both-pass.
   `--analyze-evidence` n=13 ineligible; ITT mean=-0.077 LCL=-0.214;
   tokens A 1266552.4 / C 996995.8; both-pass n=2 C-A=+69213.5. Not a
   gate. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `scikit-learn-11310` (2026-08-15):** A 48-round cap;
   B/C gold passed (C 48r forgotten 66 recovered 0). First A=0 C=1 pair.
   `--analyze-evidence` n=14 ineligible; ITT mean=0 LCL=-0.186
   `degenerate=false`; tokens A 1327902.6 / C 1022527.8. Not a gate. Not
   mixed into P0 / 403 / diag.
   **P1 after-proxy `scikit-learn-13496` (2026-08-15):** A gold passed;
   B/C 48-round cap. Second A=1 C=0 pair. `--analyze-evidence` n=15
   ineligible; ITT mean=-0.067 LCL=-0.275; tokens A 1282476.1 /
   C 1059177.3. Not a gate. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `scikit-learn-14894` (2026-08-15):** A `verify_failed`
   7r `28616`; B gold passed; C 48-round cap. `--analyze-evidence` n=16
   ineligible; ITT mean=-0.062 LCL=-0.256; tokens A 1198885.4 /
   C 1055563.8. Not a gate. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `sphinx-8548` (2026-08-15):** B/C `verify_failed`
   (C 2r); A HTTP 502 `upstream_error`. `--analyze-evidence` n=17
   ineligible; cost-missing 2/17. Treat 502 as relay jitter. Not mixed
   into P0 / 403 / diag.
   **P1 after-proxy `sympy-22914` (2026-08-15):** B/C 429
   `DAILY_LIMIT_EXCEEDED`; A 502. `--analyze-evidence` n=18 ineligible;
   cost-missing 3/18. Quota stop. Not mixed into P0 / 403 / diag.
   **P1 after-proxy `sympy-22914` retry (2026-08-16):** A/C gold passed
   (C 11r forgotten 25 recovered 0); B `verify_failed`. Third both-pass.
   `--analyze-evidence` n=18 ineligible; ITT mean=-0.056; tokens A
   1141378.4 / C 999071.7; both-pass n=3 C-A=+3781. Not a gate. Not
   mixed into P0 / 403 / diag.
   **P1 after-proxy `django-11749` (2026-08-16):** A `verify_failed` 6r;
   B/C empty-assistant (`model_in=0`). `--analyze-evidence` n=19
   ineligible; ITT mean=-0.053; cost-missing 3/19. Not mixed into P0 /
   403 / diag.
   **P1 after-proxy `django-11999` (2026-08-16):** C gold passed 32r
   forgotten 39 recovered 0; A `verify_failed` 2r; B turn timeout.
   Second A=0 C=1 pair. `--analyze-evidence` n=20 ineligible; ITT mean=0
   LCL=-0.177; tokens A 1074592.0 / C 981106.8. Not a gate. Not mixed
   into P0 / 403 / diag.
   **P1 after-proxy `django-12708` (2026-08-16):** A/B/C gold passed (C
   18r `374119` forgotten 31 recovered 0). Fourth both-pass.
   `--analyze-evidence` n=21 ineligible; ITT mean=0 LCL=-0.168
   `degenerate=false`; cost-missing 3/21; tokens A 1066469.9 /
   C 947385.2; both-pass n=4 C-A=-135733. Frozen SWE-bench n=1 in this
   dir is complete (still repeats=1). Not a gate. Not mixed into P0 /
   403 / diag.
   **P1 after-episode-distill n=1 (2026-08-16):**
   `p1-after-episode-distill` (not the after-proxy ITT). `recall_after_fix`
   B pass 15r; A/C `verify_failed` both 17r (C missing `4B`); C search 0
   recovered 5/18. `js-ms-negative-parse` A/B/C pass (C 6r = A 6r). Extra
   C rounds on js-ms stay gone. Not a gate. Not mixed into P0 / 403 /
   diag / after-proxy.
   **Follow-up 2026-08-16:** file-only eval workspaces get a local git
   seed so `git.status` is a real probe. Do not hide the tool.
   File-only 81-cell retrieval secondaries (`--file-only --pilot-calibrate`
   on `pilot-30`): search calls 0.2/0.2; C forgotten/recovered 11.3/0.9.
   Do not retune scoring. Do not amend n. Do not mix P0/P1 ITT tables.
   **P1 file-only 9×3 (2026-08-16):** `target/eval-evidence/p1-file-only-calibrate`
   (not `pilot-30`, not after-proxy). `--file-only --pilot-calibrate`
   `decision=pilot` coverage 9/30 cells 81/270; ITT A=B=C=0.889;
   diagnostic C−A mean=0 LCL=0 `degenerate=true`. `--analyze-evidence`
   ineligible n=9; SPEC hash unchanged. 72 pass / 9 `verify_failed`;
   `uuid-parity-keys` 0/9 all arms (hidden `cargo test --offline`);
   other 8 tasks 9/9. Cost-missing 0/27; tokens A 62899.6 / C 69826.4
   C−A=+6926.8 rounds 8.9/9.5. Retrieval search 0.1/0.0; C
   forgotten/recovered 14.7/1.4 vs A 0/0. Not a gate. Remaining
   calibration: frozen SWE-bench 21×3 (after-proxy is n=1). Do not
   retune scoring. Do not amend n. Do not mix P0/P1 ITT tables.

### NeedVerify roles + unpinned surface (2026-08-19)

`--compare-live recall_after_fix`, pinaic `gpt-5.6-luna`, n=1, dirty
tree on `105465d` plus execution / `ToolSpec.roles`. Bundles:
`crates/agent-eval/evidence/roles-verify-recall/` (`REPORT.md`, pinned
vs unpinned cells). Not mixed into P0 / 403 / Context Bench.

The C-hygiene leftover on this fixture was C **23r/35t** vs A 16r/17t
(git verify + re-reads). This tree:

| engine | pinned (git/shell still always_loaded) | unpinned (catalog only) |
| --- | --- | --- |
| append | 5/8, 70537 in, 14r, 15 tools, git+shell 7 | **8/8**, 101314, 17r, 16 tools, git+shell 0 |
| rolling | 7/8, 78936, 15r, 19, git+shell 8 | 5/8, 75599, 14r, 16, git+shell 0 |
| dynamic | 7/8, 87817, 15r, 19, git+shell 8 | 7/8, 70425, 14r, 13, git+shell 0 |

C extra rounds/tools improved (23/35 → 15/19 pinned → **14/13**
unpinned). Per-round schema 1578 → 1333. Hidden checks stay mixed (C
missed `visit_all` then `4B`). Total `model_in` is not a same-trace A/B
(append unpinned spent three extra rounds). No builtin `Verify` role
yet; `capability.manage` is already MustSurface. Not a gate. Do not
retune scoring.

Until those artifacts and the predeclared interval exist, the live tables are
diagnostic observations rather than independently reproducible acceptance
evidence. M15, V2, learned/vector policy and transport-selection gates remain
closed; M12/M13 continue on their independent trusted-execution route.

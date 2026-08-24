# Pending tool-cohort long-flow repeat 7 (2026-08-24)

Status: **complete mixed counterexample; lease thrash fixed mechanically,
overall convergence did not improve**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the dirty source identity recorded in each manifest. It is the
first pair after model-explicit tool loads became pending-use sources until
exact use, unload, or directive end.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 62 | 59 | 3 | 8 | 249,180 | 66,431 | 32,250 | 3,344 |
| Append A | 4/4 | 46 | 35 | 2 | 8 | 290,214 | 125,690 | 86,744 | 18,554 |

C kept its Context advantage: model input -14%, historical context -47%,
selected tokens -63%, final resident bytes -82%, and total provider tokens
-11%. Both arms passed all four hidden assertions and C's maximum turn fell
from r6's 15 to 8. Overall execution still missed the gate: C used sixteen
more rounds, twenty-four more calls, one extra failed output, and 38% more wall
time. This is not convergence acceptance.

The intended lease mechanism did work. C's r6 Git churn fell from seven
`git.status`, two `git.diff`, and fifteen `capability.manage` calls to one,
one, and ten respectively. In the already-satisfied decode turn, load receipts
for `git.diff` and `git.status` coexisted; both were retained, called once, and
released together. No inspect/load/reload loop recurred. Deterministic cohort
tests and these events support retaining the source-lifetime correction as a
tool-lifecycle correctness fix, but not claiming it as a sufficient execution
optimization.

The residual call gap is broader: C/A used `capability.manage` 10/4,
`fs.list` 6/1, `fs.read` 17/12, `search.grep` 4/2, Git reads 2/0,
and patch/replace calls 9/6. Evidence-only results were 39/19 and the maximum
result streak without an outcome advance was 7/3. Eight of C's ten catalog
calls addressed ordinary coding primitives (`fs.write`, `edit.replace`,
`git.status`, `git.diff`), with one inspect and one mistaken recipe search.
This shows that treating compact universal file-write and Git-review schemas
as cold capabilities imposes control rounds larger than their schema cost.

The trace also isolates a separate edit-expression problem. While adding a
roundtrip helper, the model supplied the whole `encode` function as `old` and
only the helper as `new`. `edit.patch` correctly performed the exact requested
replacement, thereby deleting `encode`; verification caught it, and the model
reread and restored the function. This was not an anchor miss, stale revision,
filesystem failure, or transaction error. A future explicit insertion mode is
a generic editor-interface candidate, but is not mixed into the next surface
measurement.

Post-run surface candidate changes only the compact production core: add
`fs.write` (78 schema tokens), `git.status` (47), and `git.diff` (65) to the
stable model surface. The resulting core is about 947 tokens, below the
unchanged 4,096-token cap. Shell/process, `edit.replace`, Context/task control
and plugins remain catalog-loaded. Authority, approval, effect settlement,
Context selection/GC and model autonomy are unchanged. Complete
tool-runtime/runtime/eval tests and strict Clippy pass. This run predates that
surface candidate; its effect requires a new unchanged-workload pair, and the
candidate must be reverted if rounds/calls do not fall at hidden parity.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

Follow-up: [`stable core surface r8`](../longflow-stable-core-surface-r8-2026-08-24/REPORT.md)
reduced C to round parity with A (46/46) and a four-call gap while preserving
hidden parity and the Context advantage. It remains directional until an
independent repeat passes the same gate.

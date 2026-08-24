# Stable core tool-surface long-flow repeat 8 (2026-08-24)

Status: **complete directional green pair; independent repeat required before
acceptance**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the dirty source identity recorded in each manifest. It is the
first pair with compact universal file-write and Git-review tools on the
stable coding surface: `fs.write` (78 schema tokens), `git.status` (47), and
`git.diff` (65). The surface cap, Context configuration and workload are
unchanged.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 46 | 41 | 1 | 7 | 168,458 | 43,233 | 23,295 | 4,413 |
| Append A | 4/4 | 46 | 37 | 1 | 6 | 269,067 | 124,703 | 86,171 | 18,052 |

This pair meets the directional objective. C and A used the same 46 model
rounds; C's residual call gap was four, with no hidden-success or failed-output
loss and no long tail (max 7 versus 6). C used 37% fewer model-input tokens,
65% fewer historical-context tokens, 73% fewer selected tokens, 76% fewer
final resident bytes, and 34% fewer total provider tokens. Wall time remained
15% higher, so latency is not yet accepted.

The intended control-plane effect is direct. C's `capability.manage` calls
fell from ten in r7 to one, and A also used one. C's evidence-only gap fell
from 39/19 in r7 to 24/21. The additional stable schemas raised C's total
schema tokens to 50,243 versus A's 43,964, but eliminating control decisions
lowered total model input by 80,722 tokens relative to r7 and preserved a
100,609-token advantage over paired A. The cost model therefore favored the
small stable core in this pair.

The stable surface did increase direct Git use: C called status/diff 4/3
versus A's 1/1. One already-satisfied Heartbeat-test turn used seven rounds
and eight calls, including extra Git review and two patches. It did not create
r6's 15-round tail, but makes an independent repeat mandatory: the candidate
is accepted only if the repeated C-A round/call gap stays small without hidden
loss or a new tail.

No authority was widened. Surface visibility is only schema availability;
`fs.write` remains effect/approval gated and Git tools remain read-only.
Shell/process, `edit.replace`, Context/task control and plugin tools remain
catalog-loaded. The production core is about 947 schema tokens under the
unchanged 4,096-token cap. Complete tool-runtime/runtime/eval tests and strict
Clippy pass.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

Independent follow-up: [`stable core surface r9`](../longflow-stable-core-surface-r9-2026-08-24/REPORT.md)
repeated hidden parity, one catalog-control call per arm, and a small C-A
execution gap. The stable surface is retained, while the r9 editor tail keeps
formal convergence open.

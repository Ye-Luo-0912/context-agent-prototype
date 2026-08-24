# Unique-anchor long-flow repeat 4 (2026-08-24)

Status: **complete positive paired diagnostic; retain the edit hardening, keep
formal convergence acceptance open**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the source digest recorded in each dirty-tree manifest. It is the
first long-flow pair after model-visible `edit.patch` ordinal selection was
replaced by unique exact anchors and bounded edit echoes began preserving both
ends.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 53 | 51 | 0 | 7 | 208,607 | 49,326 | 23,870 | 4,255 |
| Append A | 4/4 | 54 | 44 | 2 | 13 | 333,172 | 144,563 | 100,366 | 21,747 |

C recovered from the immediately preceding 82-round / 76-call counterexample
to one fewer round than A and a bounded seven-round maximum turn. No C tool
output failed, no task was closed, and no `occurrence` field appeared in C's
trace. The result is consistent with the diagnosed ordinal-repair loop being
removed. It is still one paired repeat and therefore not a general causal
estimate.

Context retained its intended advantage: C used 37% fewer model-input tokens,
66% fewer historical-context tokens, 76% fewer selected tokens, and 80% fewer
final resident bytes. Wall time was 344 s versus A's 326 s (+5.5%). C still
used seven more calls. Six of the call gap were evidence-only results; the
tool mix included 9 versus 5 `capability.manage` calls and 3 versus 0
`artifact.read` calls.

All three C artifact reads targeted successful `verify.run` captures with zero
lines and zero bytes. The process-output contract was subsequently tightened:
zero-byte captures now return a terminal "no stdout/stderr" message without an
`artifact_ref`; any non-empty or truncated capture keeps the existing sealed
artifact behavior. This removes the invitation for those three empty reads,
but the run predates that correction, so 51 remains the measured C call count
and no synthetic subtraction is reported.

Post-run validation is green: `tool-runtime` 155/155, the four-scenario real
Runtime Convergence Bench, related Clippy, JSON/document checks, and the
zero-byte process regression. Keep Context selection/GC frozen. The next gate
is an independent unchanged-workload pair after the zero-byte-output change;
require hidden parity, no max-turn regression, and a repeated reduction in the
C/A call gap before acceptance.

The requested post-output repeat is
[`longflow-post-empty-artifact-r5-2026-08-24`](../longflow-post-empty-artifact-r5-2026-08-24/REPORT.md):
empty artifact reads disappeared and both arms passed, but the C/A call gap
widened around unrelated evidence discovery. It therefore remains a mixed
diagnostic, not acceptance.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

# Task-relevant frontier long-flow repeat 6 (2026-08-24)

Status: **complete counterexample; task-relevance advisory did not reduce
execution, formal convergence acceptance remains open**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the dirty source identity recorded in each manifest. It is the
first pair after targeted directives stopped counting novel unrooted evidence
as task-frontier progress and exact current selected file bodies gained the
`workspace_identity=current` marker.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 57 | 56 | 0 | 15 | 215,377 | 57,743 | 30,158 | 4,986 |
| Append A | 4/4 | 49 | 38 | 2 | 7 | 274,505 | 125,690 | 86,003 | 18,125 |

The Context advantage remains real: C used 21% fewer model-input tokens, 54%
fewer historical-context tokens, 65% fewer selected tokens, 72% fewer final
resident bytes, and 18% fewer total provider tokens. It nevertheless used
eight more rounds, eighteen more calls, a 15-round maximum turn, and 54% more
wall time. C recorded 42 evidence-only results versus A's 23, and its maximum
result streak without an outcome advance was 12 versus 5. The new advisory
did fire (`frontier_no_advance_peak` 8), but did not stop the repeated work.
Therefore task-frontier relevance is a truthful observability distinction,
not an accepted convergence fix.

The largest counterexample is an already-satisfied directive asking for one
Heartbeat roundtrip test. The preceding directive had proactively added that
exact test. C then spent 15 model rounds and 16 calls before reporting it was
already present and verified: one read; Git status/diff inspection and loads;
six repeated `git.status` calls; another read; verification; and another
Git-diff inspect/load/call sequence. No tool failed.

The event stream exposes a lower-level lifecycle defect. At model round 4,
`git.diff` was the only catalog-loaded optional on the model surface. The
model then loaded `git.status`; at round 5 `git.status` appeared but
`git.diff` had already disappeared. Runtime treated the next successful model
decision as consuming every previous catalog-load receipt, even when that
decision merely loaded a cooperating sibling tool. This decision-bound
single-tool surface creates inspect/load/reload thrash and prevents a model
from assembling a small task-specific cohort. The same mechanism is present
in the earlier targeted no-op turn.

Post-run correction is source-driven and has no round TTL. Runtime now keeps
two orthogonal turn-local sets: an explicitly loaded tool remains pending
until that exact tool is called, explicitly unloaded, or the directive ends;
a called tool remains rooted only through delivery of its result to the next
successful decision. Sequential loads can therefore coexist, while use
consumes only the matching pending root and turn completion releases unused
members. The set never enters Context or checkpoints, host/operator loads
remain a separate persistent source, the 4,096-token surface cap is unchanged,
and no tool call or autonomous exploration is blocked.

Deterministic validation covers `load A -> load B -> call A -> call B ->
finish`, the pre-existing single-tool load/call/release path, directive-start
cleanup, task roots and audit failures. The complete `agent-runtime` suite
passes (235 unit, 48 actor, 4 approval, 27 host, 30 instance, 3 recall and 43
turn tests), with strict Clippy and formatting green. This run predates the
cohort correction; do not subtract calls or claim its live effect. The next
gate is one unchanged-workload pair on the corrected code, requiring hidden
parity, retained Context advantage, and no new maximum-turn tail.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

Follow-up: [`pending tool-cohort r7`](../longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md)
confirmed the Git lease thrash disappeared, but overall C execution remained
above A; the lease correction is retained as lifecycle correctness, not
accepted as a sufficient convergence fix.

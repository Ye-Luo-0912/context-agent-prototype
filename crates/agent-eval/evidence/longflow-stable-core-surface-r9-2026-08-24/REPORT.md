# Stable core tool-surface independent repeat 9 (2026-08-24)

Status: **complete repeat supports retaining the surface boundary; execution
convergence and M15 remain open**.

This is an independent direct PinAI/Luna Responses repeat of r8 on the
unchanged production `late_constraint_long` workload, one concurrent A/C
pair, complete provider usage, and the dirty source identity recorded in each
manifest. The production core again included compact `fs.write`, `git.status`
and `git.diff` schemas under the unchanged 4,096-token cap.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 49 | 46 | 2 | 9 | 211,560 | 49,632 | 23,385 | 4,508 |
| Append A | 4/4 | 47 | 38 | 1 | 7 | 269,742 | 127,480 | 89,252 | 19,116 |

The central result repeated. Both arms passed all hidden assertions; C stayed
within two rounds and eight calls of A, and `capability.manage` remained one
call in each arm. C used 22% fewer model-input tokens, 61% fewer historical-
context tokens, 74% fewer selected tokens, 76% fewer final resident bytes and
17% fewer total provider tokens. Compared with pre-surface r7, C fell from
62 rounds / 59 calls to 49 / 46 while paired A moved from 46 / 35 to 47 / 38.

Across r8+r9 the stable-core pairs are C/A 46/46 and 49/47 rounds, with 41/37
and 46/38 calls. The median difference is +1 round and +6 calls, versus r7's
+16/+24. Hidden success is 4/4 in all four arm-runs and the Context advantage
remains large. This supports retaining the stable core boundary rather than
returning universal coding primitives to the catalog control loop.

It does not close execution convergence. r9's C maximum turn was 9 versus A's
7, and C had one additional failed output. The nine-round Hello turn contained
one `edit.patch` `no_exact_match`: the model emitted two sequential hunks that
both targeted the same ping test, so the first hunk removed the second hunk's
anchor. Recovery needed rereads and three patch calls. This is the remaining
editor-expression tail, not catalog load churn or a filesystem transaction
failure. Wall time was also 46% above A.

Post-run editor correction makes hunk intent explicit. The model-visible
`edit.patch` shape now requires `op: replace | insert_before | insert_after`;
insert operations preserve a unique exact anchor and require explicit
separator/newline content. Legacy omitted `op` remains parser-only `replace`
compatibility. Revision checks, exact matching, CRLF/mixed-EOL preservation,
multi-file settlement and size bounds are unchanged. Deterministic tests cover
before/after insertion, anchor preservation, CRLF adaptation and legacy calls.
This run predates that interface and cannot establish its live effect.

Formal M15 remains open: these are two dirty-tree diagnostic pairs, not the
frozen acceptance suite, and the editor long tail still needs its own unchanged
live repeat after deterministic coverage.

Follow-up: [`explicit edit operations r10`](../longflow-explicit-edit-ops-r10-2026-08-24/REPORT.md)
completed that unchanged live repeat. Both arms passed hidden 4/4; C/A was
48/47 rounds and 41/39 calls with the same failed-output count and max turn.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

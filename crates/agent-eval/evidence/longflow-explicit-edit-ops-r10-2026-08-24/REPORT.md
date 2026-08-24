# Explicit edit-operation long-flow repeat 10 (2026-08-24)

Status: **complete unchanged live repeat supports retaining explicit edit
operations and the stable core; formal M15 remains open**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the dirty source identity recorded in each manifest. It is the
first pair after the model-visible `edit.patch` hunk contract began requiring
`op: replace | insert_before | insert_after`. The Context policy, surface cap,
stable tool set and workload were unchanged from r9.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 48 | 41 | 3 | 8 | 181,417 | 46,768 | 22,869 | 4,066 |
| Append A | 4/4 | 47 | 39 | 3 | 8 | 299,249 | 140,105 | 102,018 | 24,764 |

The convergence result is directionally green. C finished within one model
round and two tool calls of A, with identical hidden success, failed-output
count and maximum turn. C used 39% fewer model-input tokens, 67% fewer
historical-context tokens, 78% fewer selected tokens, 84% fewer final resident
bytes and 36% fewer total provider tokens. C wall time was still 8% higher
(273.6 s versus 253.9 s), so latency remains open.

The editor contract was exercised rather than merely exposed. C issued six
patch calls containing explicit insertion operations; four committed and two
failed closed. The Ping helper, the first test module, the RELEASE line and
the final Heartbeat roundtrip insertion all preserved their anchors. The r9
Hello failure, where one replacement consumed a later hunk's anchor, did not
recur: the r10 C Hello turn completed in five rounds / five calls with one
successful four-hunk patch, versus r9's nine-round / ten-call recovery tail.
Because the provider trajectory is stochastic, that comparison supports the
interface but does not by itself prove causality.

The two C patch refusals were safe locator failures, not filesystem or
settlement failures:

- one `ambiguous_match` used a closing-brace anchor that appeared twice;
- one `no_exact_match` included trailing whitespace not present in the file.

Both returned the current revision and bounded candidate windows, left the
workspace unchanged, and recovered through a new exact anchor. The first
recovery did include an over-broad successful replacement which removed the
Hello test; a proactive read noticed it and a later exact patch restored the
test before verification. This is model anchor/patch construction debt, not
evidence of an unsafe mutation transaction. A also had three failures: one
ambiguous patch and two compile-verification exits.

C's third failed output was an initial `fs.read` of the requested but absent
`NOTES.md`; the typed path response showed the workspace topology and the
model correctly switched to `fs.write`. It was not an edit or filesystem
settlement failure.

Across stable-core r8, r9 and r10, C/A round pairs are 46/46, 49/47 and 48/47;
call pairs are 41/37, 46/38 and 41/39. The median gap is now +1 round and +4
calls, versus pre-surface r7's +16/+24. Every arm passed hidden 4/4, while C
kept a large Context/input advantage in all three pairs. This supports
retaining both the compact stable coding surface and explicit insert
operations.

Do not add positional or fuzzy edit authority from this one remaining
ambiguous sample. A future measured candidate may add bounded, revision-bound
exact guard context for retrying repeated anchors, but only if unchanged live
evidence shows that ambiguous-anchor rereads remain a material tail. It must
preserve exact matching, current-revision CAS, bounded output, EOL fidelity and
transaction settlement.

Formal M15 remains open: r8-r10 are dirty-tree diagnostic pairs, not the
frozen acceptance suite. They establish a product-direction decision, not the
pre-registered acceptance claim.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`append/events.jsonl`](late_constraint_long/r1/append/events.jsonl)
- [`pair.json`](late_constraint_long/r1/pair.json)

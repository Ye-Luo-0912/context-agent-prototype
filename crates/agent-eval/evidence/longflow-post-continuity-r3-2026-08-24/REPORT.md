# Post-continuity long-flow repeat 3 (2026-08-24)

Status: **complete paired counterexample; both hidden outcomes pass, execution
convergence fails**.

This direct PinAI/Luna Responses run used the production tool surface, one
concurrent `late_constraint_long` A/C pair, complete provider usage, and a
dirty source tree whose digest is recorded in each manifest. It ran after the
task-continuity and exact-verification slices and immediately before the
unique-anchor/edit-echo hardening described below.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 82 | 76 | 5 | 30 | 415,897 | 47,126 | 4,017 |
| Append A | 4/4 | 47 | 36 | 0 | 6 | 291,034 | 92,974 | 19,528 |

Context itself remained lighter in C: selected tokens were 49% lower, final
resident bytes 79% lower, and historical-context tokens 24% lower. Extra
rounds reversed the whole-task input result (+43% C versus A), so the Context
advantage is real but insufficient while execution amplification remains.

## Root-cause trace

The excess was concentrated rather than uniform. One v2 Hello edit turn used
30 rounds / 31 calls / 4 failed outputs in C versus 5 / 5 / 0 in A. C's first
four-hunk `edit.patch` matched and committed, but its final hunk replaced an
existing test plus the module terminator with a new test that omitted the
terminator. `verify.run` truthfully returned Rust's unclosed-delimiter error.
Subsequent repairs repeatedly supplied low-uniqueness `}` anchors with
`occurrence: 1`; those exact edits landed on earlier braces and alternated
between two broken revisions. Three verification failures were real failed
checks, not process/tool transport defects. The turn eventually recovered by
rewriting the bounded file and passed verification.

The old success echo amplified recovery cost: the changed span crossed nearly
the whole file, but the 1,200-character prefix-only cap stopped at line 35 and
hid the final file tail. Across the complete run C used 18 edit attempts,
23 `fs.read` calls, 10 `verify.run` calls, 10 `capability.manage` calls, and 233
tool lifecycle transitions. Those counts are consequences of the long repair
trajectory; task completion remained zero in both arms, so the previously
fixed task-closure loop did not recur.

## Retained correction and gate

The production edit contract now omits ordinal `occurrence` from the
model-visible `edit.patch` schema and requires a unique exact anchor with
enough unchanged context. The legacy field remains parser-compatible. Bounded
success echoes preserve both the beginning and end of a large changed span,
and the global multi-file cap likewise marks a middle omission rather than
silently dropping the tail. Matching remains exact; revision CAS, transaction
staging, line-ending handling, Context selection, GC, and model autonomy are
unchanged.

Deterministic `tool-runtime` (154 tests), `agent-eval` (129 tests), and Clippy
are green. A post-correction live `add_test` compatibility smoke also passed in
4 rounds / 3 calls / 0 failures; its first `edit.patch` committed, with no
confirm read or shell/whole-file-write fallback. The long-flow run predates the
correction, so that short smoke does not establish a live causal reduction of
the 30-round tail. Keep the task-continuity and edit-hardening candidates, but
keep the success/round/call gate open. The next provider-backed long-flow test
must use the unchanged generic workload and require both hidden success and no
new max-turn tail.

The first such post-hardening pair is now available in
[`longflow-post-edit-anchor-r4-2026-08-24`](../longflow-post-edit-anchor-r4-2026-08-24/REPORT.md):
both arms passed 4/4, C recovered to 53 rounds / 51 calls / max-turn 7 versus
A's 54 / 44 / 13. It is strong directional evidence, but one pair does not
close the formal gate.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

# Zero-output long-flow repeat 5 (2026-08-24)

Status: **complete mixed paired diagnostic; zero-output routing validated,
formal convergence acceptance remains open**.

This direct PinAI/Luna Responses run used the unchanged production
`late_constraint_long` workload, one concurrent A/C pair, complete provider
usage, and the dirty source identity recorded in each manifest. It is the
first pair after zero-byte `process.run` / host-owned `verify.run` captures
stopped publishing an `artifact_ref`.

| Arm | Hidden | Rounds | Calls | Failed outputs | Max turn | Model input | Historical context | Selected tokens | Final resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Dynamic C | 4/4 | 47 | 44 | 1 | 6 | 192,571 | 44,694 | 22,049 | 4,559 |
| Append A | 4/4 | 43 | 32 | 0 | 6 | 243,064 | 104,286 | 72,095 | 19,011 |

The zero-output change behaved as designed: C made no `artifact.read` call,
versus three empty verification-artifact reads in r4. This is observed final
code behavior, not a synthetic subtraction. Both hidden arms remained 4/4.

The broader convergence result is mixed. C used 21% fewer model-input tokens,
57% fewer historical-context tokens, 69% fewer selected tokens, 76% fewer
final resident bytes, and 16% fewer total provider tokens. It nevertheless
used four more rounds, twelve more tool calls, and 23% more wall time. Relative
to r4, both arms became shorter, so the wider call gap cannot be attributed to
the zero-output change alone.

The twelve-call gap is exactly concentrated in evidence/discovery behavior.
C versus A used `capability.manage` 8/2, `search.grep` 4/0, `fs.list` 5/2,
Git reads 2/0, `edit.replace` 1/0, `edit.patch` 5/7, and `fs.read` 10/12;
both used two writes and seven verifications. C recorded 29 evidence-only
results versus A's 16. Its only failed output loaded
`rust.compile-tests:src/protocol.rs` through `capability.manage`, confusing a
`verify.run` recipe value with a tool name, then recovered through catalog
search.

One targeted no-op turn explains most of the amplification. For “Refactor
decode …” C already received a selected current `src/protocol.rs@revision`
body and TaskProgress identity, then spent six rounds and ten calls on a
second read, grep, list, three capability loads, two Git reads, capability
search, and verification before reporting that the requested Result behavior
already existed. A used one read and one verification. The existing Evidence
Frontier classified globally novel directory/Git facts as progress, allowing
unrelated novelty to reset convergence debt even after the directive had an
exact current target.

Post-run correction is intentionally generic and advisory-only. A new user
directive records whether it already has an exact Fresh task-rooted resource.
Once it does, novel unrooted observations remain in the bounded evidence table
but do not advance the task convergence frontier; broad directives without an
exact target keep the old exploration semantics. Selected exact file bodies
now co-locate `workspace_identity=current`, and `verify.run` /
`capability.manage` schemas state that recipe ids are argument values, never
loadable tool names. No Context selection, GC, residency, token budget, tool
availability, or execution block changed.

Post-correction validation: `agent-runtime` 389 tests and `tool-runtime` 156
tests pass, including targeted-vs-open exploration, exact current-body
projection, and recipe/tool namespace regressions. This run predates that
task-relevance correction; do not subtract calls or claim its effect. The next
gate is an unchanged-workload pair on the corrected code, requiring hidden
parity and a repeated reduction of C's rounds/calls without losing the Context
advantage.

Raw authorities:

- [`dynamic/summary.json`](late_constraint_long/r1/dynamic/summary.json)
- [`dynamic/events.jsonl`](late_constraint_long/r1/dynamic/events.jsonl)
- [`append/summary.json`](late_constraint_long/r1/append/summary.json)
- [`pair.json`](late_constraint_long/r1/pair.json)

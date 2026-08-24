# Tool-contract follow-up long-flow (2026-08-24)

## Decision

Do not count this attempt as a paired performance result and do not add a
second repeat. Dynamic C passed hidden verification but remained above its
prior directional baseline at 107 rounds / 112 calls. Append A timed out on
fixture turn 6 and passed only 1/4 hidden assertions, so the pair is severely
and asymmetrically censored.

The run used direct PinAI `gpt-5.6-luna`, native Responses, the production
tool surface, the same `late_constraint_long` spec, and a dirty source tree.

| Metric | Dynamic C | Append A |
| --- | ---: | ---: |
| Hidden assertions | 4/4 | 1/4 |
| Outcome | passed | turn-6 timeout |
| Rounds | 107 | 14 |
| Tool calls | 112 | 9 |
| Failed outputs | 33 | 3 |
| Usage complete | yes | no |
| Max turn rounds | 23 | 5 before timeout |

## Contract findings

The operation-aware `context.manage` parser worked: all 4 calls succeeded,
versus 17 failures in 18 calls in the preceding negative run. This supports
the deterministic fix, but it is not evidence of fewer whole-task calls.

The stricter cursor schema did not work. Every `fs.list` call (19/19) and
every `search.grep` call (6/6) failed. Rather than omit the optional cursor,
the model fabricated values shaped to the published regex, including
`artifact://v1/draft/...#0`, `artifact://v1/workspace#0`, and placeholder
digests. The backend correctly rejected them because no such run-owned
artifact existed. Five repeated exact fake-list arguments and eight
`process.run` calls show the fallback cost, but a single stochastic trace
cannot assign every extra round to that path.

This is stronger evidence than the earlier empty-value observation: an
optional opaque capability on a frequently used first-page tool invites the
model to invent authority. More prose, `minLength`, and a shape regex cannot
establish provenance.

## Retained route

Use one model-visible artifact continuation primitive. `fs.list`,
`search.grep`, and `code.symbols` still emit bounded first-page results and a
run-owned `artifact_ref` when they overflow; further lines are read through
the already-always-loaded `artifact.read {reference,start_line,end_line}`.
Their legacy snapshot cursor remains parser-only compatibility for trusted
non-model callers, but is absent from the model schema. Results expose the
next artifact line number and name `artifact.read` directly.

This removes three optional opaque-capability fields and reuses an existing
bounded tool. It does not change Context, GC, retrieval budgets, tool-call
limits, or model autonomy. Deterministic tests prove schema convergence and
legacy paging behavior. Live acceptance remains at least two valid paired
repeats with equal hidden success, lower median C rounds/calls, and no new
p95/max-turn tail.

The trace also isolated completion evidence: C made 16 `task.complete` calls
and five failed because model-supplied artifact claims were invalid. Runtime
already binds the current assistant-output artifact and current verification
refs into `CompletionRecord`, so the optional artifact-echo field is now
parser-only compatibility as well. The model-visible completion contract is a
bounded semantic summary only. This deterministic correction is not included
in the live result above and still needs the same paired gate.

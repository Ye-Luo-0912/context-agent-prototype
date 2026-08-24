# Empty-cursor normalization negative experiment (2026-08-24)

## Decision

Reject and revert the runtime behavior that treated an empty or `"0"` paging
cursor as an omitted cursor. It removed the visible `fs.list`/`search.grep`
cursor failures, but it did not reduce tool invocations and the Dynamic C cell
expanded sharply. Append A timed out and missed one hidden assertion, so this
is not a valid paired comparison; even the C-only result fails the directional
performance gate.

The run used direct PinAI `gpt-5.6-luna`, native Responses, the production tool
surface, and the same `late_constraint_long` fixture/spec as the preceding
diagnostic. The source tree was dirty.

| Metric | Previous C | Normalized-cursor C | Change |
| --- | ---: | ---: | ---: |
| Hidden assertions | 4/4 | 4/4 | equal |
| Rounds | 75 | 137 | +83% |
| Tool calls | 85 | 171 | +101% |
| Model input | 323,199 | 708,278 | +119% |
| Wall time | 412,230 ms | 772,641 ms | +87% |
| `fs.list` + `search.grep` failures | 25 | 0 | removed |
| `fs.list` + `search.grep` calls | 25 | 31 | increased |
| Max turn rounds | 12 | 47 | new long tail |

Append A ended on fixture turn 13 after 47 rounds / 39 tool calls and timed
out. Hidden verification passed 3/4; `Heartbeat` was absent. Its usage is
incomplete and token counts are lower bounds, so no C/A percentage from this
attempt is valid.

## What the experiment exposed

Silent cursor normalization converted invalid pagination into successful
first-page reads. It improved the error counter while preserving the model's
redundant invocation and made repeated exploration less visible. That violates
the optimization criterion: the goal is fewer calls/rounds at equal success,
not merely fewer red tool results.

The longer C trace also exposed a separate `context.manage` union-parser
defect: 17/18 calls failed. Eleven failed before operation dispatch because
unused optional properties contained empty strings or because unenumerated
`kind`/`scope` values were guessed; six later searches hit the existing
per-turn query budget. This is independent of cursor semantics.

## Correction after the follow-up run

Runtime cursor normalization remains reverted. A follow-up strict-schema run
showed that publishing a canonical cursor regex instead caused the model to
fabricate matching-looking artifact identities. The final retained surface
therefore removes per-tool cursor properties from the model schema and routes
all spill continuation through `artifact.read`; legacy per-tool cursors remain
parser-only and fail-closed for non-model compatibility.

`context.manage` now keeps union text fields raw until `op` is known and parses
only fields consumed by that operation. Unused empty placeholders are ignored,
while required empty values, bad UUIDs, invalid relevant enums, and unknown ops
still fail. The schema exposes the exact bounded `ContextKind` and
`ContextScope` vocabularies. This is a generic tool-contract correction; it
does not alter Context retrieval budgets, selection, GC, or model autonomy.

See `../longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md`.

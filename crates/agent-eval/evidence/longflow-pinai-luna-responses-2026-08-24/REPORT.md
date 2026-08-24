# PinAI/Luna Responses long-flow diagnostic (2026-08-24)

## Decision

Dynamic C retained a large Context-size advantage and both arms passed all
four hidden assertions, but C still amplified execution: 10 more model rounds,
28 more tool calls, 20 more failed tool outputs, and 62 seconds more wall time
than Append A. This is evidence to continue the execution/tool-contract route,
not a reason to retune Context selection or GC.

The run used direct PinAI `gpt-5.6-luna` through the native Responses protocol,
the production-default tool surface, one `late_constraint_long` repeat, and a
dirty source tree. Provider usage is complete in both cells. It is a diagnostic,
not a clean-tree M15 acceptance run or a success-rate estimate.

| Metric | Dynamic C | Append A | C relative to A |
| --- | ---: | ---: | ---: |
| Hidden assertions | 4/4 | 4/4 | equal |
| Rounds | 75 | 65 | +15% |
| Tool calls | 85 | 57 | +49% |
| Failed tool outputs | 29 | 9 | +222% |
| Wall time | 412,230 ms | 349,983 ms | +18% |
| Model input | 323,199 | 492,321 | -34% |
| Selected resident + reactivated tokens | 20,594 | 154,681 | -87% |
| Historical-context prompt tokens | 43,821 | 217,072 | -80% |
| Final resident bytes | 580 | 27,436 | -98% |
| Tool-schema prompt tokens | 72,145 | 54,409 | +33% |

The Context advantage is therefore still real and material. The cost is in
execution amplification and repeated optional-tool exposure, not in C carrying
more working context.

## Failure anatomy

Twenty-five of C's 29 failed outputs came from one pagination contract:
`fs.list` failed 14/14 and `search.grep` failed 11/11. Nineteen requests sent
an empty or `"0"` cursor; six invented a non-canonical artifact identity.
Those calls were invalid first-page/pagination requests, not filesystem I/O
failures. The remaining failures were two missing-path reads, one malformed
`context.manage` call, and one process exit.

This concentration explains the failure-rate gap but does not by itself prove
that accepting malformed cursors would reduce rounds or calls. A tool invocation
that is made redundant by a bad optional argument remains a call even if the
backend silently normalizes it.

## Route

The next two diagnostics showed that neither accepting empty/zero nor exposing
a stricter cursor regex is sufficient: the former masks provenance, while the
latter caused the model to fabricate regex-shaped artifact identities. The
retained route is one model-visible continuation primitive: bounded first-page
tools emit `artifact_ref`, and `artifact.read` reads further lines. Legacy
per-tool cursors stay parser-only. Separately, parse union-shaped meta-tool
arguments after operation dispatch so an unused empty placeholder cannot
poison an otherwise valid operation.

Acceptance for either change remains execution-level: hidden success cannot
fall, and paired live repeats must reduce median rounds and calls without a new
p95/max-turn tail. A lower failed-output counter alone is insufficient.

Follow-up evidence:

- `../longflow-pinai-luna-cursor-normalized-2026-08-24/REPORT.md`
- `../longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md`

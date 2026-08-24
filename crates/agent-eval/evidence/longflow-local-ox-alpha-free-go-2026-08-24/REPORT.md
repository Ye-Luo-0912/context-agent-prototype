# Local OX Alpha Free long-flow diagnostic (2026-08-24)

## Decision

Exclude this pair from Context comparisons. Dynamic reached hidden verification
but failed it; Append terminated on a structurally empty completion after a
provider link failure had been rewritten as a successful `stop`. The cells are
therefore neither a paired success nor a valid cost comparison.

| Arm | Outcome | Model rounds | Tool calls | Failed tools | Wall ms |
| --- | --- | ---: | ---: | ---: | ---: |
| Append | error | 24 | 17 | 0 | 265,578 |
| Dynamic | verify_failed | 42 | 32 | 5 | 893,176 |

The diagnosed transport defect was that `finish_reason=network_error` lost its
failure semantics and became an empty success. The provider and local relay now
preserve that signal as retryable transport failure. OpenCode Go's native
`/responses` path also returns an opaque HTTP 500 when unavailable; the relay
now converts only that exact shape into HTTP 501 so an `auto` client can cache a
same-local-provider Chat fallback. It never falls over to PinAI.

Post-fix short-gate evidence is deliberately not merged into this old bundle:
the live `add_test` fixture passed on `ox-alpha-free` with 8 model rounds,
7 tool calls, 0 failed tool outputs, and first-attempt committed editing.


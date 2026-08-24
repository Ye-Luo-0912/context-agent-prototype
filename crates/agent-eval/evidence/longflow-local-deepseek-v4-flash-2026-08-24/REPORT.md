# Local DeepSeek V4 Flash long-flow diagnostic (2026-08-24)

## Decision

Exclude this pair from Context and execution-coherence comparisons. Both arms
hit the same provider protocol incompatibility before completing the fixture:
Console Go required thinking-mode `reasoning_content` to be replayed, while the
Chat adapter had no bounded representation for that vendor-only field.

| Arm | Outcome | Model rounds | Tool calls | Failed tools | Wall ms |
| --- | --- | ---: | ---: | ---: | ---: |
| Append | error | 12 | 10 | 0 | 39,859 |
| Dynamic | error | 12 | 14 | 0 | 39,174 |

The equal failure class is provider-wire evidence, not evidence for or against
dynamic Context. Do not use the token or call deltas as a performance result.
The current route preference is native Responses where supported; otherwise a
provider-specific compatibility decision must be made without adding vendor
thinking state to Runtime Context authority.


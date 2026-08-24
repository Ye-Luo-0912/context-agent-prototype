# Exact-verification long-flow retry (2026-08-24)

Status: **inconclusive external-failure diagnostic; excluded from every A/C
performance, success-rate, wall-time, and token comparison**.

This was one independent two-repeat retry after the first provider-failed
attempt. It used the same production long-flow path and wrote dirty-tree source
digests. All four arm-runs again ended on retryable provider transport errors.

| Repeat | Arm | Outcome | Partial rounds | Partial tool calls | `verify.run` started / OK | Usage |
| --- | --- | --- | ---: | ---: | ---: | --- |
| r1 | append | provider transport error | 15 | 12 | 1 / 1 | incomplete, lower bound |
| r1 | dynamic | provider transport error | 1 | 0 | 0 / 0 | incomplete, lower bound |
| r2 | append | provider transport error | 4 | 4 | 0 / 0 | incomplete, lower bound |
| r2 | dynamic | provider transport error | 1 | 0 | 0 / 0 | incomplete, lower bound |

C lost its first model request in both repeats while A progressed farther
before the same external failure. This is asymmetric censoring: the small C
round/tool counts are not improvements and the larger A counts are not
regressions. Hidden checks are red because the trajectory stopped early.

The only arm that reached `verify.run` produced a successful real process PASS
and one `ExecutionVerificationPass(kind=recorded)` event. No equivalent second
request occurred, so this retry does not measure exact-PASS reuse. The hardened
deterministic Convergence Bench remains green and is the current mechanism
authority.

Decision: stop provider-backed reruns until transport is stable. Then collect
at least two complete paired repeats on a clean source tree; require all hidden
checks green before comparing C/A rounds, calls, context, tokens or wall time.

Raw pair authorities:

- [`r1/pair.json`](late_constraint_long/r1/pair.json)
- [`r2/pair.json`](late_constraint_long/r2/pair.json)

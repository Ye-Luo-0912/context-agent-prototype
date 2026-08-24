# Exact-verification long-flow attempt (2026-08-24)

Status: **inconclusive external-failure diagnostic; excluded from performance,
success-rate, and token comparisons**.

## Run identity

- Command: `agent-eval --repeats 2 --allow-dirty --longflow-run late_constraint_long`
- Arms: append (A) and dynamic (C), concurrent within each repeat.
- Production surface: conditional host-registered `verify.run` was present.
- Source identity: each manifest records a bounded `source_tree_digest`; this
  was an explicitly dirty-tree diagnostic, not candidate evidence.

## Result

| Repeat | Arm | Outcome | Partial rounds | Partial tool calls | `verify.run` started / OK | Usage |
| --- | --- | --- | ---: | ---: | ---: | --- |
| r1 | append | provider transport error | 11 | 9 | 1 / 1 | incomplete, lower bound |
| r1 | dynamic | provider transport error | 5 | 6 | 0 / 0 | incomplete, lower bound |
| r2 | append | provider transport error | 5 | 5 | 0 / 0 | incomplete, lower bound |
| r2 | dynamic | provider transport error | 6 | 6 | 1 / 1 | incomplete, lower bound |

All four arm-runs ended on the same retryable failure while sending a model
request to the configured provider. Hidden verification is red because the
15-turn trajectory stopped early, not because a completed candidate regressed.
The partial round/tool totals are therefore censored and must not be compared.

The two arms that reached `verify.run` each produced one successful real
process result and one `ExecutionVerificationPass(kind=recorded)` event. No arm
requested the same verifier a second time before the provider failure, so this
attempt neither confirms nor contradicts live exact-PASS reuse. The deterministic
real-runtime Convergence Bench remains the current mechanism evidence: two
equivalent requests yield one process start, two truthful terminal results, and
Recorded/Reused = 1/1.

## Decision

- Keep the implementation and deterministic evidence.
- Do not update the C/A round, tool-call, success, wall-time, or token baseline
  from this attempt.
- Preserve Context/GC policy unchanged. This run contains no completed pair
  capable of re-measuring the existing C context advantage.
- Re-run at least two paired repeats only after provider transport is stable;
  candidate evidence must also use a clean source tree and pass hidden checks.

Raw pair authorities:

- [`r1/pair.json`](late_constraint_long/r1/pair.json)
- [`r2/pair.json`](late_constraint_long/r2/pair.json)

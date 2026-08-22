# Tool Surface Edit diagnostic — revision/header follow-up

Status: historical dirty-tree diagnostic only. This run still persisted
`agent-eval.tool-surface-edit.v1`; it predates the v2 pack and v3 gate.

## Scope and result

- 4 fixtures × 3 repeats = 12 production-default cells.
- Raw-byte hidden verification: 12/12.
- Persisted v1 flow gate: 10/12.
- Patch attempts: 14; failures: 2, both non-stale `InvalidRequest` calls.
- Forbidden fallback calls: 0; confirmation reads: 0.
- Model rounds: 44; `fs.read` bytes: 534.
- Wall time: 195,247 ms total; current nearest-index p50/p95 recomputation is
  15,082/20,189 ms.
- Reported provider tokens: 272,533 total; current nearest-index p50/p95 is
  19,007/30,282.
- Recovery-required or unknown commit settlements: 0.

## What the two failures showed

In `crlf_multi_hunk/r1` and `mixed_eol/r1`, the first model call used a
`files[]` entry with `base_revision` and `hunks` but omitted that entry's
`path`. The runtime correctly rejected the malformed call; the next model
round used the legacy top-level shortcut and completed the edit. This was
direct evidence that presenting two edit shapes created avoidable first-call
errors, and motivated exposing only the canonical `files[]` form to the
model while retaining the top-level parser only for compatibility.

The stale fixture did reread current bytes and completed with one patch, but
this old gate did not bind that route to the model-invisible fixture mutation
event boundary. It therefore remains diagnostic rather than acceptance.

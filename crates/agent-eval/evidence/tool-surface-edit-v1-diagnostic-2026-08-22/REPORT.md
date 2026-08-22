# Tool Surface Edit diagnostic — baseline

Status: historical dirty-tree diagnostic only. This run used
`agent-eval.tool-surface-edit.v1`; its persisted gate results must not be
silently recomputed with the later v3 analyzer.

## Scope and result

- 4 fixtures × 3 repeats = 12 production-default cells, fixed dynamic
  context engine, one recorded model identity.
- Raw-byte hidden verification: 12/12.
- Persisted v1 flow gate: 5/12.
- Patch attempts: 15; failures: 3, all typed stale refusals.
- Forbidden fallback calls: 2 (`shell.exec`); confirmation reads: 0.
- Model rounds: 55; `fs.read` bytes: 655.
- Wall time: 273,109 ms total; current nearest-index p50/p95 recomputation is
  17,815/39,788 ms.
- Reported provider tokens: 346,448 total; current nearest-index p50/p95 is
  25,249/43,565.
- Recovery-required or unknown commit settlements: 0.

## Reading

The final bytes prove that the tasks eventually completed, but the extra
patch failures, shell fallback, and 55 rounds show that the original edit
surface was not yet a reliable primary path. This run predates the single
model-visible `files[]` schema, revision provenance gate, exact local-hunk
contract, and fixture-mutation event boundary.

This evidence is a baseline for diagnosis, not acceptance and not proof of a
general task-failure rate.

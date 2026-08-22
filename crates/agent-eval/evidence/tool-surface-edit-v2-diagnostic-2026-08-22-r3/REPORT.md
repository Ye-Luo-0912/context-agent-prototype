# Tool Surface Edit v2/v3 diagnostic

Status: **diagnostic pass**, not formal acceptance. The run used
`--allow-dirty`; `run-plan.json` and all 12 manifests bind the exact tested
tree with source digest
`8610971a56f57726dae8d943e5da9ba11bbd95ee5388dc99920985cc59052052`.

## Frozen scope

- Pack: `agent-eval.tool-edit.v2`, digest
  `e5c871282743a079c1e7b85be11859b05cd86e49a7cb8aa44587ae2a6b586611`.
- Gate: `agent-eval.tool-surface-edit.v3`; its implementation digest is
  recorded in the run plan, summary, and every cell sidecar.
- 4 fixtures × 3 repeats = 12 cells; production-default tool surface;
  fixed dynamic engine; recorded model `gpt-5.6-luna`.
- The model-visible `edit.patch` schema digest is recorded in
  `run-plan.json`; it exposes only revision-checked `files[]`.
- The v3 gate joins every patch revision to the latest successful same-path
  `fs.read`, verifies exact bounded local-hunk fingerprints, binds the stale
  route to the model-invisible fixture mutation boundary, and requires raw
  SHA-256 file truth plus complete runtime barriers.

## Result

| Measure | Result |
| --- | ---: |
| Raw-byte hidden verification | 12/12 |
| v3 flow gate | 12/12 |
| Non-conflict valid-call first attempt | 9/9 |
| Stale route | 3/3 proactive; 0 reactive |
| Patch attempts / changed successes | 12 / 12 |
| Patch failures / stale refusals | 0 / 0 |
| Revision-provenance / target / hunk violations | 0 / 0 / 0 |
| Forbidden fallback / post-success confirm reads | 0 / 0 |
| Recovery-required / unknown settlements | 0 / 0 |
| Model rounds | 42 |
| `fs.read` bytes | 534 |
| Wall time total; p50 / p95 | 176,472 ms; 15,206 / 18,586 ms |
| Provider tokens total; p50 / p95 | 258,773; 18,965 / 30,235 |
| Usage completeness | 12/12 complete; not a lower bound |
| Run/cell identity checks | all pass; one model and one source identity |

Every CRLF and mixed-EOL cell used one read plus one successful patch. Every
two-file cell used two reads plus one composite patch. Each stale cell first
read the seed revision, crossed the recorded external-mutation boundary,
reread the concurrent revision, then committed one patch that preserved the
concurrent line.

## Historical comparison

The tables use the current nearest-index percentile formula over persisted
cell summaries. All three runs recorded the same model and endpoint, but their
source digests differ; r3 also changed the pack, model-visible schema, and
gate. This is a directional diagnostic, not a controlled causal estimate of
one filesystem algorithm.

| Run | Strict | Persisted gate | Non-conflict first patch | Patch success / attempts | Fallback | Rounds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline v1 | 12/12 | 5/12 | 3/9 | 12/15 | 2 | 55 |
| Follow-up r2 v1 | 12/12 | 10/12 | 7/9 | 12/14 | 0 | 44 |
| This run v2/v3 | 12/12 | 12/12 | 9/9 | 12/12 | 0 | 42 |

| Run | Wall p50 / p95 | Wall total | Tokens p50 / p95 | Tokens total | Tool calls | `fs.read` bytes | Edit-to-green p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline v1 | 17.815 / 39.788 s | 273.109 s | 25,249 / 43,565 | 346,448 | 43 | 655 | 8.366 s |
| Follow-up r2 v1 | 15.082 / 20.189 s | 195.247 s | 19,007 / 30,282 | 272,533 | 32 | 534 | 5.836 s |
| This run v2/v3 | 15.206 / 18.586 s | 176.472 s | 18,965 / 30,235 | 258,773 | 30 | 534 | 0.039 s |

Relative to baseline, r3 used 23.6% fewer rounds, 25.3% fewer reported
tokens, 35.4% less total wall time, and a 53.3% lower wall p95. Relative to
r2, total time and wall p95 improved while wall p50 rose 124 ms (about 0.8%);
the evidence therefore does not show that every latency statistic improved.

The strongest supported conclusion is narrow: on this frozen 12-cell edit
surface, the combined v2 pack, canonical schema, v3 gate, and current runtime
eliminated the observed malformed/stale edit calls and fallbacks while
preserving exact bytes and reducing total rounds, total time, and reported
tokens relative to both diagnostics. All three runs already ended with 12/12
correct raw bytes, so the improvement is call quality and efficiency rather
than a transition from incorrect to correct final files. This is sufficient
evidence to continue the file-edit reliability work and to use `edit.patch`
as the primary mutation path in the prototype.

## Limits and next gate

- Dirty-tree, one-model, 4×3 evidence is not a statistical estimate of the
  project's general task-failure rate and cannot close M12, M13, or M15.
- It does not exercise a direct or authority-bypassing filesystem writer
  racing the hash-to-replace window, crash recovery, disk-full/journal faults,
  rollback cleanup failure, or ACL/alternate-stream preservation. A second
  official `Workspace::open` on the same root is normally refused by the
  authority-log lock; that is not the residual race.
- Multi-file application remains sequential rather than cross-file atomic;
  honest partial recovery remains part of the contract.
- No recovery/unknown settlement occurred here; absence in 12 cells does not
  prove those paths unreachable.
- Staged-byte cost is not yet aggregated. Mixed-EOL canonicalization is
  bounded but has not been shown to be a performance hotspot.
- A formal Tool Surface acceptance run requires the same frozen pack on a
  clean source tree. Add deterministic race/fault injection and staged-byte
  accounting before making broader filesystem reliability claims.

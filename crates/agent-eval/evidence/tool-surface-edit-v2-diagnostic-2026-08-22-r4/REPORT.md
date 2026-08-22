# Tool Surface Edit v2/v3 diagnostic — r4

Status: **diagnostic pass**, not formal acceptance. This run used
`--allow-dirty`; the plan, all 12 manifests, and the completed run summary
bind the tested tree to source digest
`2d6b052f74682112f52af1814136e04cea9b5240322cfa98f1657766e97ce87a`.
`acceptance_eligible=false` is caused by the dirty-tree diagnostic mode, not a
failed cell.

## Frozen scope

- Pack: `agent-eval.tool-edit.v2`, digest
  `e5c871282743a079c1e7b85be11859b05cd86e49a7cb8aa44587ae2a6b586611`.
- Gate: `agent-eval.tool-surface-edit.v3`, implementation digest
  `2a25cfbf29190c0171dc06f09a03b4376ec2a8586a994e6721ac627e4cf4d030`.
- Model-visible `edit.patch` schema digest:
  `03f67eee2ac0f71f762e0d27f5ef64cad2df6bc0b01887ee30b6a1ebed91db10`;
  it exposes only revision-required `files[]`.
- 4 fixtures × 3 repeats = 12 cells; production-default tool surface;
  dynamic engine; recorded model `gpt-5.6-luna`.
- Independent post-run inspection found 12 manifests, 12 gate sidecars and 12
  tool-edit sidecars; every manifest source digest equals the plan, all gates
  pass, and no cell records a violation.

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
| Model rounds / tool calls | 42 / 30 |
| `fs.read` bytes | 534 |
| Wall time total; p50 / p95 | 164,417 ms; 13,507 / 15,616 ms |
| Provider tokens total; p50 / p95 | 258,325; 18,961 / 30,171 |
| Edit-to-green p95 | 39 ms |
| Usage completeness | 12/12 complete; not a lower bound |
| Run/cell identity checks | all pass; one model and one source identity |

Every CRLF and mixed-EOL cell used one read and one successful patch. Every
two-file cell used two reads and one composite patch. Each stale cell read the
seed revision, crossed the recorded external-mutation boundary, proactively
reread the concurrent revision, then committed one patch preserving the
concurrent line. No cell attempted the stale revision first.

## Comparison with r3

R3 and r4 use the same pack, v3 gate, model-visible `edit.patch` schema,
recorded model and endpoint, but bind different source-tree digests. R4 binds
the later source containing typed rollback settlement, structured Runtime
cleanup-recovery projections, authority journal v2, bounded/verified crash
reconciliation, recovery-target locking, the aligned 4 MiB read/edit ceiling,
and the rule that file effects never create parent directories implicitly.

| Run | Strict / gate | First patch | Patch success | Rounds / calls | Wall total; p50 / p95 | Tokens total; p50 / p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| r3 | 12/12 · 12/12 | 9/9 | 12/12 | 42 / 30 | 176.472 s; 15.206 / 18.586 s | 258,773; 18,965 / 30,235 |
| r4 | 12/12 · 12/12 | 9/9 | 12/12 | 42 / 30 | 164.417 s; 13.507 / 15.616 s | 258,325; 18,961 / 30,171 |

R4 preserved every correctness and call-quality result. Observed total wall
time was 6.8% lower than r3, wall p50 11.2% lower, and wall p95 16.0% lower;
reported token totals and percentiles were effectively unchanged (all within
0.3%). Edit-to-green p95 stayed 39 ms. This is evidence of no visible
regression on these small success/stale fixtures, not a causal performance
estimate: provider/network variance was not controlled and the pack does not
measure 4 MiB hashing or durability barriers directly.

## What the evidence supports

The narrow conclusion is now stronger than r3: the hardened file transaction
and recovery code preserved 12/12 exact final bytes, 12/12 correct flows, zero
malformed/stale patch attempts and the same minimal call count on a source-
bound rerun. It remains reasonable to continue the project and keep
`edit.patch` as the primary model-facing mutation path. The evidence shows
call-quality and workflow reliability on this surface; it does not estimate a
general coding-task failure rate.

## Limits and next gate

- Dirty-tree, one-model, 4×3 evidence cannot close M12, M13 or M15. Formal Tool
  Surface acceptance still requires this frozen pack on a clean source tree.
- These live cells do not execute process crashes, partial writes, disk-full,
  journal-terminal failures, cleanup failure, a direct filesystem writer
  racing hash→replace, or partial multi-file recovery. Deterministic unit tests
  cover the new prepare crash seams and conservative cleanup, but are not a
  substitute for fault-injected end-to-end evidence.
- A second official `Workspace::open` on the same root is refused by the
  authority-journal lock. Direct or authority-bypassing filesystem writers,
  Unix's narrow check→rename window, Windows directory-sync limits, and
  incomplete ACL/metadata preservation remain explicit boundaries.
- Multi-file application is sequential, not cross-file atomic. No
  recovery/unknown settlement occurred here, so zero observations do not make
  those states unreachable.
- Add test-only staged/read/hash/journal/sync counters for 4 KiB, 256 KiB and
  4 MiB, single/two/16-file cases before removing any integrity pass or making
  broader high-performance claims.

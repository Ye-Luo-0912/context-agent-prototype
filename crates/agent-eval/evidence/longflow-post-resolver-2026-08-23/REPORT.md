# Longflow after ProgramResolver / obligation lineage / cache dormancy (2026-08-23)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com), repeats=2 on a clean tree
(git `115a18e`). All four arm-runs completed and passed hidden
verification (file_content + command, 4/4 asserts each).

This run validates the third-review fixes end to end on live
trajectories: the host-owned ProgramResolver (TOOL-PROC-01), the
obligation lineage with precondition epochs (CONV-03), event-visible
ledger accounting (CONV-OBS-01), and suspend-instead-of-delete body
cache semantics (PROTO-EVID-03).

## Results

| metric | C r1 | A r1 | C r2 | A r2 |
| --- | --- | --- | --- | --- |
| passed | ✅ | ✅ | ✅ | ✅ |
| wall_ms | 390799 | 337620 | 512039 | 230956 |
| rounds | 58 | 50 | 73 | 37 |
| tool calls | 71 | 52 | 109 | 33 |
| model input tokens | 487292 | 541454 | 656211 | 388523 |
| process.run calls/failed | 13/**1** | 7/**0** | 26/6 | 0/0 |
| frontier_advances | 41 | 32 | 66 | 26 |
| redundant_evidence_calls | 1 | 1 | 0 | 1 |
| no_advance_peak | 4 | 4 | 5 | 1 |
| evidence_invalidations | 29 | 20 | 47 | 9 |
| obligations opened/attempted/precond_changed/resolved | 4/0/0/3 | 1/0/0/1 | 3/1/0/2 | 2/0/0/1 |
| avoidable_failure_calls | 0 | 0 | 1 | 0 |
| max_turn_rounds (=p95) | 8 | 8 | 15 | 5 |

Caveat: `tokens_lower_bound=true` on both A cells (provider usage
incomplete), so token comparisons carry that uncertainty.

## Finding 1 — executable-resolution waste is gone

The resolver removed exactly the failure mode it targeted. Previous
runs built multi-attempt guessing chains under stable fingerprints
(`ed466dba`×2, `f78b6ab6`×4; 10 and 9 failed launches per C cell).
This run: C r1 has **one** failed launch in 13 calls; C r2's six
failures in 26 calls are honest refusals, not resolution waste — two
`git diff --check` non-zero exits, two `cargo test`
missing_project_marker typed refusals, and one typo
(`.protocol_tests.exe`, a missing backslash) that the model
self-corrected on its very next call. Crucially, `./protocol_tests.exe`
and bare `protocol_tests.exe` — the forms that *always* failed before —
now spawn successfully, and bare-name retries after rebuilds succeed
immediately.

The ledger confirms it: `avoidable_failure_calls` = 1 across all four
cells (one repeated `cargo test` refusal under one project_marker
lineage, tracked as attempted total=2). ExecutableResolution lineages
opened and resolved cleanly (opened→resolved with matched fingerprints).

## Finding 2 — obligation events close the observability gap

Every ledger transition is in the bundles: resource_path and
executable_resolution lineages show clean opened→resolved pairs;
project_marker shows opened→attempted(total=2). Per-user-turn tail
metrics worked first run: C r2's long tail is now *visible* as
max_turn_rounds=15 inside an otherwise-normal task (73 rounds / 15
turns ≈ 4.9 avg) — exactly the §25 diagnosis target that total-round
means hid.

## Finding 3 — cache dormancy suspends but does not yet hit

Suspension is active and accounted (`suspended` = 0–13 per cell,
invalidated split out), but hit rate stayed 0 while the C arms still
logged 5 and 7 `protocol_checkpoint_body_missing` re-reads. The
three-gate conjunction for rehydration (full-frame presence ∧ truncated
from retained tail ∧ Fresh identity in TASK PROGRESS) still does not
align in these trajectories. Named suspects for the next investigation:
the bounded TASK PROGRESS `checked_files` view (many files touched per
cell), LRU capacity (≤4) against 17–24 reads, and the 8-path
BeforeModel revalidation priority cap. This is instrumentation-guided
residual work, not a correctness bug — no stale body was ever served.

## Other readings

- Rounds/tokens mixed again at n=2: A improved sharply this run (37–50
  rounds vs 47–61; input tokens −28% vs the previous run), C r2 carried
  one 15-round tail turn and 109 tool calls. Trajectory variance still
  dominates; no performance delta is claimed from n=2.
- `evidence_invalidations` stays elevated by design (tighter currentness
  retires superseded rows immediately); redundant_evidence_calls stay
  collapsed at 0–1.
- Context GC / compaction remain frozen: C peak resident stayed ~4.8 KB
  against A's historical-context tokens; compaction ~34K in / ~1K out on
  C only.

## Verdict

All four cells pass hidden verification with resolver, lineage, events,
and dormancy active. The targeted waste class (executable resolution)
measurably disappeared from the traces; ledger accounting is now fully
event-verifiable; the remaining convergence residual is precisely
localized to the body-cache rehydration gate alignment. M12/M13 remain
the mainline and stay unclosed.

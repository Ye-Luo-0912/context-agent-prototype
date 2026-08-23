# Longflow after Execution Convergence V1 (2026-08-23)

Task `late_constraint_long`, A/C concurrent, live model
(`gpt-5.6-luna` via api.pinaic.com), repeats=2 on a clean tree after a
provider outage window (history below). All four arm-runs completed
and passed the hidden verification (file_content + command, 4/4
asserts each).

## Results

| metric | C r1 | A r1 | C r2 | A r2 |
| --- | --- | --- | --- | --- |
| passed | ✅ | ✅ | ✅ | ✅ |
| rounds | 57 | 48 | 67 | 40 |
| tool calls | 72 | 46 | 94 | 33 |
| model input tokens | 470045 | 534720 | 634494 | 421340 |
| frontier_advances | 41 | 26 | 51 | 20 |
| redundant_evidence_calls | 8 | 9 | 4 | 7 |
| no_advance_peak | 6 | 4 | 4 | 3 |
| evidence_invalidations | 9 | 5 | 11 | 0 |
| fs.read motive proto-checkpoint-missing | 6 | 0 | 6 | 0 |

Per-tool highlights:

- process.run: C r1 **0 calls**, C r2 **13 / 9 failed** (a name-guessing
  chain rebuilt in that one trajectory); A 1/1 and 0/0.
- git.status + git.diff: C 12 and 11 (baseline run had 24).
- capability.manage: C 5 and 5 (baseline 7), 0 failed everywhere —
  the hysteresis reload churn stays gone.
- fs.list carries the new path@digest identity stamp; its repeats are
  counted inside `redundant_evidence_calls` (4–9 per arm-run).

## Reading against the post-hysteresis baseline
(`../longflow-post-hysteresis-2026-08-23`: C 94r/135 tools,
process.run 35/20 failed, git re-verification 24)

- n=2 per arm now. C rounds 57/67 (mean 62) vs baseline 94; A 48/40 vs
  53. Trajectories still dominate round counts — do not read mean-vs-
  single as a delta claim.
- The executable-guessing chain appeared in exactly one of four
  arm-runs (C r2: 9 failures). It is no longer the default shape of a
  C trajectory (baseline: 18 guesses in its only completed run), but it
  is not eliminated either; the convergence debt stayed below its
  advisory threshold during that chain because the model interleaved
  advancing work between guesses (peak=4 < 5).
- git status/diff re-verification collapsed from 24 to 10–12 on C
  across both runs; identity-stamped `fs.list` repeats are now visible
  as RedundantEvidence instead of invisible stdout noise.
- Per-round input cost keeps the historical shape: C ≈8–9K/round vs
  A ≈9–11K/round while carrying more rounds.
- proto-checkpoint-missing rereads stay at 6 per C run — the body
  cache removes the *need* to re-read only when the identity matches;
  the remaining six are genuine re-reads of changed/other files, not
  cache misses. No regression signal here.

## Advisory behavior observed live

`frontier_no_advance_peak` reached 6 (C r1) and 4 (others ≤4): the
EXECUTION FRONTIER UNCHANGED advisory fired at most once per run and
never stalled anything — advisory-only by design. Evidence
invalidations tracked world-revision bumps as intended (0 on the arm
with no mutations beyond edits it echoed).

## History

An earlier same-day invocation lost its r1 artifacts to a provider
503 truncation plus an overwrite during an outage retry; those numbers
were reconstructed from the harness log in the previous revision of
this report. The current tables are real artifacts from the clean
post-outage rerun above.

## Verdict

Both arms complete and passing on n=2; no structural no-progress loop
on three of four arm-runs, one bounded guessing chain on C r2 with
interleaved advances. ROADMAP's Convergence gate ("bench green AND
longflow without structural loops") reads green on the bench and
supported-but-not-closed on the longflow clause; M12/M13 remain the
mainline and stay unclosed.

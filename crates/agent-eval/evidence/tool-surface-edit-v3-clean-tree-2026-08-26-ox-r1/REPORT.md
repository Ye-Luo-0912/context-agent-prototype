# Tool-surface-edit v3 clean-tree run 6 (`-ox-r1`, cross-model) — 2026-08-26

First frozen-gate attempt against a second provider serving: the local
OpenCode relay (`ox-alpha-free`), whose independent availability smoke went
green earlier today (two live cells) and whose relay port answered before
launch. Clean tree, fresh evidence dir, 4 fixtures × 3 repeats,
production-default surface, dynamic engine.

Verdict: `verdict=fail identity=true strict=11/12 gate=6/12
non_conflict_first=8/9 stale proactive/reactive=1/0 rounds=49 wall_ms=901016
tokens=103506 lower_bound=true usage_incomplete_cells=1`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Failure reason |
| --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| crlf_multi_hunk r2/r3 | pass | pass | 1 | true | |
| mixed_eol r1/r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| mixed_eol r3 | pass | pass | 1 | true | |
| stale_revision_recovery r1 | pass | pass | 1 | true | |
| stale_revision_recovery r2/r3 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| batch_two_file r1/r2 | pass | pass | 1 | true | |
| batch_two_file r3 | n/a | fail | 0 | n/a | relay transport stream error killed the session after 2 rounds (retryable decode failure; usage incomplete, lower-bound tokens) |

## Conclusions

- The engine stayed byte-perfect on every cell that reached a tool call —
  now across two different model servings on the same day. Strict raw-byte
  truth has never failed on an applied patch.
- `ox-alpha-free` shows **perfect hunk discipline**: no non-exact first
  attempt and no wrong-revision selection anywhere, unlike Luna's windows.
  Its single non-conflict-first miss is the transport-killed cell, not a
  behavior miss.
- Its failure mode is narrower and heavier than Luna's: all five behavioral
  gate failures are the same forbidden post-edit confirmation read (5 of 11
  completed cells), where Luna spread 2–4 violations across confirmation
  reads, stale-revision selection and non-exact hunks.
- Cross-model summary after six clean-tree runs today: applied-patch
  correctness is an engine property (proven twice over); first-attempt flow
  discipline is a model property, and the post-edit confirmation read is
  the binding violation for both servings. Neither serving meets the bar
  (12/12 strict, 12/12 gate, 9/9 non-conflict-first). Closure paths from
  here are a serving/model that follows the flow contract, or an explicit
  product decision to re-scope the no-confirm rule — the latter would be a
  contract change to this gate, not a defect fix.

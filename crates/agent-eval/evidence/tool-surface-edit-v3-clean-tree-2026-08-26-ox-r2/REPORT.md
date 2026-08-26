# Tool-surface-edit v3 clean-tree run 7 (`-ox-r2`, cross-model, post-link-fix) — 2026-08-26

Re-run of the OpenCode-relay window after the transport fixes landed
(`PROV-LINK-01` in AUDIT_TODO: buffered harness replay plus the stream
idle bound). Same serving (`ox-alpha-free` via the local relay), clean
tree, fresh evidence dir, 4 fixtures × 3 repeats.

Verdict: `verdict=fail identity=true strict=12/12 gate=8/12
non_conflict_first=8/9 stale proactive/reactive=3/0 rounds=46 wall_ms=871518
tokens=97080 lower_bound=false usage_incomplete_cells=0`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Failure reason |
| --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1/r3 | pass | pass | 1 | true | |
| crlf_multi_hunk r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| mixed_eol r1/r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| mixed_eol r3 | pass | pass | 1 | true | |
| stale_revision_recovery r1–r3 | pass | pass | 1 | true | all three took the proactive route cleanly |
| batch_two_file r1/r2 | pass | pass | 1 | true | |
| batch_two_file r3 | pass | fail | 1 | false | successful patch used non-canonical hunks |

## Conclusions

- The transport fix is validated end to end against the failure that
  motivated it. Pre-fix (`-ox-r1`), one cell died on a relay stream decode
  error and the run finished `strict=11/12 … usage_incomplete_cells=1,
  lower_bound=true`. Post-fix, every stream interruption replayed and the
  run finished `strict=12/12 … usage_incomplete_cells=0, lower_bound=false`.
  Two cells carry multi-minute walls (260 s / 235 s) — the visible cost of
  an in-place replay that previously killed the cell outright.
- With transport noise removed, strict raw-byte truth is 12/12 on a second
  model serving: applied-patch correctness is now proven across two
  providers and seven windows, with zero wrong bytes ever committed.
- Remaining variance is purely model decision behavior, in exactly the
  family Luna showed: three forbidden post-edit confirmation reads and one
  successful-but-non-canonical hunk set. No stall, no truncation, no
  incomplete usage.
- The gate bar (12/12 strict, 12/12 gate, 9/9 non-conflict-first) stays
  unmet by both servings; the binding violation remains the post-edit
  confirmation read.

# Tool-surface-edit v4 clean-tree run (`-ox-r3`, post echo-contract fix) — 2026-08-26

Second archived window on the v4 echo surface, same serving
(`ox-alpha-free` via the local relay), clean tree, fresh evidence dir,
4 fixtures × 3 repeats.

Verdict: `verdict=fail identity=true strict=12/12 gate=11/12
non_conflict_first=8/9 stale proactive/reactive=3/0 rounds=42 wall_ms=593614
tokens=89372 lower_bound=false usage_incomplete_cells=0`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Confirm reads | Failure reason |
| --- | --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1–r3 | pass | pass | 1 | true | 0 | |
| mixed_eol r1–r3 | pass | pass | 1 | true | 0 | |
| stale_revision_recovery r1–r3 | pass | pass | 1 | true | 0 | all proactive route |
| batch_two_file r3 | pass | fail | 1 | false | 0 | successful patch merged each file's two anchor lines into one multiline hunk |
| batch_two_file r1/r2 | pass | pass | 1 | true | 0 | |

## Conclusions

- Replicates `-ox-r2` exactly: strict 12/12, zero post-edit confirmation
  reads anywhere, wall ~10 min, and exactly one intermittent gate miss on
  `batch_two_file` hunk granularity (this time r3 instead of r1). Across
  the two archived v4 windows the confirm-read violation is 0 of 24 cells;
  the pre-fix serving produced it in every window on both providers.
- The residual is stable in shape and rate (~1 of 3 repeats per window)
  and is a grader-contract question, not a regression: the merged-hunk
  patch is byte-perfect and revision-correct. Closing the last cell needs
  a decision — teach the canonical granularity in model-visible text, or
  accept byte-equivalent granularities in `exact_hunks`.
- A third same-day window ran before these two with the echo fix already
  active but flags mis-ordered (`--evidence-dir` after `--tool-edit-run`),
  so no artifacts were persisted; its console verdict was strict 12/12,
  gate 12/12, non-conflict-first 9/9 — a full-bar pass that cannot serve
  as archival evidence.

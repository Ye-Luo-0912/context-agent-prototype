# Tool-surface-edit v4 clean-tree run (`-ox-r2`, post echo-contract fix) — 2026-08-26

First archived window after making the no-confirm edit contract visible:
the `edit.patch` success echo header now reads "patch applied and
committed; this echo is final, no re-read needed" instead of the bare
"patch applied". Root cause and motivation are recorded in AUDIT_TODO:
the rule lived only in grader config since the v3 surface compaction
dropped it from the tool description under the 96-char cap.

Serving: `ox-alpha-free` via the local OpenCode relay, clean tree at the
echo-contract commit, fresh evidence dir, 4 fixtures × 3 repeats.

Verdict: `verdict=fail identity=true strict=12/12 gate=11/12
non_conflict_first=8/9 stale proactive/reactive=3/0 rounds=42 wall_ms=508726
tokens=87773 lower_bound=false usage_incomplete_cells=0`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Confirm reads | Failure reason |
| --- | --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1–r3 | pass | pass | 1 | true | 0 | |
| mixed_eol r1–r3 | pass | pass | 1 | true | 0 | |
| stale_revision_recovery r1–r3 | pass | pass | 1 | true | 0 | all proactive route |
| batch_two_file r1 | pass | fail | 1 | false | 0 | successful patch merged each file's two anchor lines into one multiline hunk |
| batch_two_file r2/r3 | pass | pass | 1 | true | 0 | |

## Conclusions

- The binding violation is gone. Post-edit confirmation reads were the
  cross-provider blocker in every prior window (5 cells in v3 `-ox-r1`,
  3 in v3 `-ox-r2`, plus Luna's); all 12 cells here ran `confirm=0`.
  The visibility fix did exactly what the root-cause analysis predicted.
- Cost dropped with the wasted round-trips: wall 509 s vs 871 s in the
  pre-fix window, rounds 42 vs 46.
- The single gate miss is the known second-order shape: byte-perfect,
  revision-correct patch whose hunk granularity differs from the golden
  decomposition (two anchors merged into one multiline hunk per file).
  The fixture text never states the required granularity; only the
  grader's `exact_hunks` does. That is the same hidden-contract family
  this window's fix addressed, but closing it means either teaching the
  canonical granularity (fixture/surface wording change) or re-scoping
  `exact_hunks` to accept byte-equivalent decompositions — both are gate
  contract decisions, not engine defects.

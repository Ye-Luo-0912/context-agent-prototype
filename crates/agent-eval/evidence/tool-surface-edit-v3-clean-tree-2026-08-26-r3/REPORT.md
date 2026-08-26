# Tool-surface-edit v3 clean-tree run 4 (`-r3`) — 2026-08-26

Fourth clean-tree frozen-gate attempt, launched immediately after a green
availability smoke (`fix_off_by_one` live, `first_raw_ok=1/1`). Healthy
provider window throughout (wall 570 s, no session loss,
`usage_incomplete_cells=0`). Provider PinAI `gpt-5.6-luna` direct,
4 fixtures × 3 repeats, production-default surface, dynamic engine.

Verdict: `verdict=fail identity=true strict=12/12 gate=9/12
non_conflict_first=8/9 stale proactive/reactive=1/0 rounds=45 wall_ms=570181
tokens=284697`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Failure reason |
| --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1–r3 | pass | pass | 1 | true | |
| mixed_eol r1–r3 | pass | pass | 1 | true | |
| stale_revision_recovery r1 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| stale_revision_recovery r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| stale_revision_recovery r3 | pass | pass | 1 | true | |
| batch_two_file r1/r3 | pass | pass | 1 | true | |
| batch_two_file r2 | pass | fail | 2 | false | first attempt non-exact hunks and failed; recovered second attempt |

## Conclusions

- The separation established by runs 1–3 holds exactly. Strict raw-byte
  truth is 12/12 again: every applied patch was byte-perfect. The mutation
  path (engine, revision binding, fingerprints, commit) has never produced a
  wrong byte across all four clean-tree runs.
- All gate variance remains model decision behavior, in the same two shapes:
  optional post-edit confirmation reads forbidden by the
  `stale_revision_recovery` contract (2 of 3 repeats this window — note the
  same fixture passed cleanly with `confirm=0` on its third repeat), and one
  non-exact first hunk set on `batch_two_file`, recovered on the second
  attempt.
- The gate bar (12/12 strict, 12/12 gate, 9/9 non-conflict-first) stays
  unmet by this provider serving; TOOL-EDIT-02 stays open honestly. The
  fluctuation pattern across windows (gate 9→8→9 here vs. 8 previously)
  confirms first-attempt reliability is a provider/model-serving property,
  not an engine defect.

# Tool-surface-edit v3 clean-tree run 5 (`-r4`) — 2026-08-26

Fifth clean-tree frozen-gate attempt, launched after a green availability
smoke in a healthy window (wall 218 s — fastest of the day — no session
loss, `usage_incomplete_cells=0`). Provider PinAI `gpt-5.6-luna` direct,
4 fixtures × 3 repeats, production-default surface, dynamic engine.

Verdict: `verdict=fail identity=true strict=12/12 gate=9/12
non_conflict_first=8/9 stale proactive/reactive=1/0 rounds=45 wall_ms=218221
tokens=285053`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Failure reason |
| --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1–r3 | pass | pass | 1 | true | |
| mixed_eol r1–r3 | pass | pass | 1 | true | |
| stale_revision_recovery r1 | pass | pass | 1 | true | |
| stale_revision_recovery r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| stale_revision_recovery r3 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| batch_two_file r1 | pass | fail | 2 | false | first attempt non-exact hunks and failed; recovered second attempt |
| batch_two_file r2/r3 | pass | pass | 1 | true | |

## Conclusions

- Strict raw-byte truth is 12/12 for the fourth consecutive complete
  window: every applied patch was byte-perfect across all five clean-tree
  runs today (60 cells; the single strict miss ever was the cell whose
  session died before any tool call).
- Gate variance stays inside the same two model-behavior shapes seen all
  day: forbidden post-edit confirmation reads on `stale_revision_recovery`
  (2 of 3 repeats here) and one non-exact first-hunk set on
  `batch_two_file`, recovered on the second attempt.
- The pattern has saturated: five independent windows produced gate
  scores of 9/8/9/9/12-failing cells with identical failure taxonomy.
  Further same-day retries against this serving add no information.
  TOOL-EDIT-02 stays open for a materially different provider or model
  serving that can hold first-attempt discipline across all twelve cells;
  its bar remains 12/12 strict, 12/12 gate, 9/9 non-conflict-first.

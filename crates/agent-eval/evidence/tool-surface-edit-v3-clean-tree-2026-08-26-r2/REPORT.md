# Tool-surface-edit v3 clean-tree run 2 — 2026-08-26 (third attempt overall)

Post-fix rerun in a healthy provider latency window (wall 280 s, no session
loss, `usage_incomplete_cells=0`). Provider PinAI `gpt-5.6-luna` direct,
4 fixtures × 3 repeats, production-default surface, dynamic engine.

Verdict: `verdict=fail identity=true strict=12/12 gate=8/12
non_conflict_first=7/9 rounds=47 wall_ms=280462 tokens=299031`.

Per-cell outcomes:

| Cell | Strict | Gate | Attempts | First exact | Failure reason |
| --- | --- | --- | --- | --- | --- |
| crlf_multi_hunk r1 | pass | fail | 2 | true | first patch used revisions not from latest reads; recovered |
| crlf_multi_hunk r2/r3 | pass | pass | 1 | true | |
| mixed_eol r1–r3 | pass | pass | 1 | true | |
| stale_revision_recovery r1 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| stale_revision_recovery r2 | pass | fail | 1 | true | one post-edit confirmation read (fixture forbids) |
| stale_revision_recovery r3 | pass | pass | 1 | true | |
| batch_two_file r1/r2 | pass | pass | 1 | true | |
| batch_two_file r3 | pass | fail | 2 | false | first attempt non-exact hunks and failed; recovered |

## Conclusions

- Across all three clean-tree runs (36 cells), every applied patch was
  byte-perfect: strict raw-byte truth never failed except the one cell whose
  provider session died before any tool call. The mutation path itself is
  solid.
- All gate variance is model decision behavior: stale-revision selection,
  optional post-edit confirmation reads, and occasional non-exact first
  hunks. The current `gpt-5.6-luna` serving does not reliably produce
  first-attempt canonical patches, so TOOL-EDIT-02's bar (12/12 strict,
  12/12 gate, 9/9 first-patch) is not met by this provider window.
- Combined with the previous bundle (`tool-surface-edit-v3-clean-tree-
  2026-08-26/`), the diagnostic separation holds: engine correctness is
  proven; first-attempt reliability is a provider/model-serving property
  that fluctuates between windows.

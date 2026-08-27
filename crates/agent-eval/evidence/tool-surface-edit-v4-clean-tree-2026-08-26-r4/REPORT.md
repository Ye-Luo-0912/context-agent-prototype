# Tool-surface-edit v4 clean-tree run (`Luna direct, gate v4`) — 2026-08-26

First archival 4×3 window on the versioned gate `agent-eval.tool-surface-edit.v4`
which accepts byte-equivalent hunk decompositions. Product contract is
committed byte/revision/settlement truth; hunk partition is not model-visible
authority.

Provider: PinAI direct `gpt-5.6-luna` via `https://api.pinaic.com/v1`,
clean tree at `040e65f` plus gate docs `34cb2de`, fresh evidence dir.

Verdict: `acceptance_pass identity=true strict=12/12 gate=12/12
non_conflict_first=9/9 stale proactive/reactive=3/0 rounds=42 wall_ms=190794
tokens=263986 lower_bound=false usage_incomplete_cells=0`.

Per-cell outcomes: all 12 cells passed with first-attempt exact or
byte-equivalent hunks, zero confirmation reads and zero fallback.

| Fixture | Repeats | Strict | Gate | Confirm |
| --- | --- | --- | --- | --- |
| batch_two_file | 3 | 3/3 | 3/3 | 0 |
| crlf_multi_hunk | 3 | 3/3 | 3/3 | 0 |
| mixed_eol | 3 | 3/3 | 3/3 | 0 |
| stale_revision_recovery | 3 | 3/3 | 3/3 | 0 |

This window satisfies the ordered route requirement: one archival 4×3
confirmation on the versioned gate after the byte-equivalent change.
Prior windows on the same provider and code scored 11/12, 9/12 and 10/12
due to first-attempt non-exact hunks or confirm reads; this run shows the
frozen surface meets the product contract without requiring a golden
decomposition.

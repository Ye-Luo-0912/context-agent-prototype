# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | fail | pass | failed | n/a | healthy | 7 | 14 | 61518 | fail |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 13 | 21 | 155576 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 11 | 24 | 104348 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 13 | 26 | 141294 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 15 | 69507 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 9 | 15 | 62206 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 10 | 21 | 65377 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 12 | 21 | 74868 | pass |
| retry_policy_dev normal r1 | fail | pass | failed | n/a | healthy | 13 | 27 | 212799 | fail |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 12 | 25 | 130669 | pass |
| retry_policy_dev resume r1 | fail | pass | failed | failed | healthy | 6 | 13 | 147155 | fail |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 30 | 52 | 265976 | pass |

Summary: pass 9/12; NOT_RUN 0/12; behavior pass 9/12; closures 2/12.

Efficiency facts: rounds total/max 146/30; tool calls total/max 274/52; wall max 265976 ms; provider input/output tokens 1086351/139190 (cached input 193664); schema tokens 151378. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

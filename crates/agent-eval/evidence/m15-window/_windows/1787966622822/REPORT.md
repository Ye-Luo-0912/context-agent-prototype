# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | completed | n/a | healthy | 11 | 23 | 112150 | pass |
| retry_diag_dev normal r2 | pass | pass | completed | n/a | healthy | 6 | 16 | 77767 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 9 | 19 | 92788 | pass |
| retry_diag_dev resume r2 | fail | pass | active | restored_and_continued | healthy | 11 | 25 | 125018 | fail |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 8 | 21 | 79901 | pass |
| retry_migrate_dev normal r2 | pass | pass | completed | n/a | healthy | 5 | 14 | 44222 | pass |
| retry_migrate_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 12 | 25 | 392352 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 11 | 22 | 712990 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 15 | 40 | 148215 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 12 | 30 | 202208 | pass |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 21 | 49 | 269087 | pass |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 16 | 48 | 473004 | pass |

Summary: pass 11/12; NOT_RUN 0/12; behavior pass 11/12; closures 8/12.

Efficiency facts: rounds total/max 137/21; tool calls total/max 332/49; wall max 712990 ms; provider input/output tokens 1408538/59419; schema tokens 143568. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

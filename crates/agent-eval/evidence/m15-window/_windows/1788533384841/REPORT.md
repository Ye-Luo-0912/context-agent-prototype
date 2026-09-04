# M15 development window — FAILED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 9 | 16 | 139615 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 8 | 15 | 93400 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 12 | 20 | 142565 | pass |
| retry_diag_dev resume r2 | fail | pass | active | restored_and_continued | healthy | 11 | 23 | 151729 | fail |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 9 | 19 | 99333 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 7 | 20 | 108126 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 10 | 21 | 130244 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 11 | 27 | 137588 | pass |
| retry_policy_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 20 | 154078 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 29 | 58 | 245683 | pass |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 21 | 53 | 214425 | pass |
| retry_policy_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 34 | 69 | 436186 | pass |

Summary: pass 11/12; NOT_RUN 0/12; behavior pass 11/12; closures 2/12.

Efficiency facts: rounds total/max 171/34; tool calls total/max 361/69; wall max 436186 ms; provider input/output tokens 1661925/64834 (cached input 133632); schema tokens 176547. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

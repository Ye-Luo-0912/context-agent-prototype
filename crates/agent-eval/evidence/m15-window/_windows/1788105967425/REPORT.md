# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 15 | 86432 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 13 | 20 | 105574 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 11 | 23 | 102526 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 14 | 29 | 114581 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 8 | 14 | 60048 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 9 | 15 | 63799 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 25 | 87096 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 12 | 24 | 85810 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 20 | 37 | 173500 | pass |
| retry_policy_dev normal r2 | fail | pass | failed | n/a | healthy | 6 | 12 | 116323 | fail |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 24 | 47 | 295092 | pass |
| retry_policy_dev resume r2 | fail | pass | failed | failed | healthy | 4 | 9 | 88386 | fail |

Summary: pass 10/12; NOT_RUN 0/12; behavior pass 10/12; closures 2/12.

Efficiency facts: rounds total/max 144/24; tool calls total/max 270/47; wall max 295092 ms; provider input/output tokens 1051395/117154 (cached input 140160); schema tokens 152314. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

# M15 development window — FAILED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | fail | pass | active | n/a | healthy | 7 | 11 | 91696 | fail |
| retry_diag_dev normal r2 | fail | pass | active | n/a | healthy | 8 | 12 | 100314 | fail |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 10 | 26 | 122518 | pass |
| retry_diag_dev resume r2 | fail | pass | active | restored_and_continued | healthy | 47 | 60 | 324693 | fail |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 21 | 98731 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 10 | 21 | 99508 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 11 | 24 | 130482 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 10 | 20 | 107667 | pass |
| retry_policy_dev normal r1 | pass | pass | failed | n/a | healthy | 48 | 98 | 478154 | fail |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 20 | 43 | 208389 | pass |
| retry_policy_dev resume r1 | fail | pass | failed | restored_and_continued | healthy | 9 | 27 | 389348 | fail |
| retry_policy_dev resume r2 | pass | pass | failed | restored_and_continued | healthy | 53 | 125 | 504292 | fail |

Summary: pass 6/12; NOT_RUN 0/12; behavior pass 8/12; closures 1/12.

Efficiency facts: rounds total/max 243/53; tool calls total/max 488/125; wall max 504292 ms; provider input/output tokens 2441871/81827 (cached input 217600); schema tokens 257195. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

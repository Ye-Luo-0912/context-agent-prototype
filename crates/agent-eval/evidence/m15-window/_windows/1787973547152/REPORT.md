# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 7 | 13 | 89076 | pass |
| retry_diag_dev normal r2 | fail | pass | active | n/a | healthy | 7 | 10 | 88914 | fail |
| retry_diag_dev resume r1 | fail | pass | active | restored_and_continued | healthy | 9 | 18 | 97046 | fail |
| retry_diag_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 9 | 19 | 78930 | pass |
| retry_migrate_dev normal r1 | pass | pass | completed | n/a | healthy | 9 | 24 | 76596 | pass |
| retry_migrate_dev normal r2 | pass | pass | completed | n/a | healthy | 10 | 23 | 73011 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 31 | 135330 | pass |
| retry_migrate_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 9 | 20 | 369171 | pass |
| retry_policy_dev normal r1 | pass | pass | active | n/a | healthy | 17 | 42 | 184532 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 19 | 44 | 175208 | pass |
| retry_policy_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 21 | 56 | 237600 | pass |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 49 | 103 | 419825 | pass |

Summary: pass 10/12; NOT_RUN 0/12; behavior pass 10/12; closures 6/12.

Efficiency facts: rounds total/max 179/49; tool calls total/max 403/103; wall max 419825 ms; provider input/output tokens 1942278/63420 (cached input 200704); schema tokens 189667. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

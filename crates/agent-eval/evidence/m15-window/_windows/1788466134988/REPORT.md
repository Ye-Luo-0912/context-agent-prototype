# M15 development window — FAILED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 17 | 108605 | pass |
| retry_diag_dev normal r2 | fail | pass | active | n/a | healthy | 6 | 9 | 71046 | fail |
| retry_diag_dev resume r1 | fail | pass | active | restored_and_continued | healthy | 9 | 17 | 103680 | fail |
| retry_diag_dev resume r2 | fail | pass | active | restored_and_continued | healthy | 9 | 19 | 99505 | fail |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 13 | 20 | 103919 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 11 | 22 | 94544 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 14 | 29 | 139127 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 12 | 25 | 146869 | pass |
| retry_policy_dev normal r1 | pass | pass | failed | n/a | healthy | 48 | 96 | 354002 | fail |
| retry_policy_dev normal r2 | pass | pass | failed | n/a | healthy | 48 | 91 | 431831 | fail |
| retry_policy_dev resume r1 | pass | pass | failed | restored_and_continued | healthy | 55 | 75 | 383045 | fail |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 39 | 83 | 424504 | pass |

Summary: pass 6/12; NOT_RUN 0/12; behavior pass 9/12; closures 1/12.

Efficiency facts: rounds total/max 274/55; tool calls total/max 503/96; wall max 431831 ms; provider input/output tokens 2759571/82221 (cached input 231424); schema tokens 296179. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

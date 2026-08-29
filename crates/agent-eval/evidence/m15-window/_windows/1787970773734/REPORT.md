# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | fail | pass | active | n/a | healthy | 5 | 9 | 62161 | fail |
| retry_diag_dev normal r2 | fail | pass | active | n/a | healthy | 6 | 12 | 78425 | fail |
| retry_diag_dev resume r1 | fail | pass | completed | restored_and_continued | healthy | 9 | 23 | 89665 | fail |
| retry_diag_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 17 | 32 | 147900 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 11 | 19 | 98232 | pass |
| retry_migrate_dev normal r2 | pass | pass | completed | n/a | healthy | 9 | 22 | 76598 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 27 | 128209 | pass |
| retry_migrate_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 10 | 24 | 77042 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 22 | 43 | 498260 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 27 | 61 | 244574 | pass |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 28 | 77 | 315456 | pass |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 26 | 59 | 254140 | pass |

Summary: pass 9/12; NOT_RUN 0/12; behavior pass 9/12; closures 8/12.

Efficiency facts: rounds total/max 183/28; tool calls total/max 408/77; wall max 498260 ms; provider input/output tokens 1943439/66144 (cached input 152576); schema tokens 189725. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

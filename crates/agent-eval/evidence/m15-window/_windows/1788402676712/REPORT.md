# M15 development window — FAILED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 6 | 11 | 82118 | pass |
| retry_diag_dev normal r2 | pass | pass | failed | n/a | healthy | 48 | 54 | 284983 | fail |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 12 | 25 | 119855 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 13 | 24 | 132360 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 7 | 15 | 90626 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 8 | 19 | 96198 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 10 | 24 | 108834 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 19 | 32 | 175504 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 36 | 68 | 309725 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 25 | 43 | 229316 | pass |
| retry_policy_dev resume r1 | pass | pass | failed | restored_and_continued | healthy | 54 | 93 | 458429 | fail |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 36 | 91 | 378308 | pass |

Summary: pass 10/12; NOT_RUN 0/12; behavior pass 12/12; closures 3/12.

Efficiency facts: rounds total/max 274/54; tool calls total/max 499/93; wall max 458429 ms; provider input/output tokens 2750601/81138 (cached input 293888); schema tokens 290539. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

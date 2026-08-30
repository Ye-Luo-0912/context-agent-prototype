# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 17 | 25 | 200575 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 14 | 21 | 124614 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 26 | 144571 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 13 | 27 | 174992 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 8 | 15 | 58067 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 9 | 16 | 75203 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 23 | 89569 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 14 | 26 | 78760 | pass |
| retry_policy_dev normal r1 | pass | pass | failed | n/a | healthy | 48 | 68 | 343771 | fail |
| retry_policy_dev normal r2 | pass | pass | failed | n/a | healthy | 48 | 78 | 462431 | fail |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 41 | 66 | 348468 | pass |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 21 | 44 | 196095 | pass |

Summary: pass 10/12; NOT_RUN 0/12; behavior pass 12/12; closures 2/12.

Efficiency facts: rounds total/max 259/48; tool calls total/max 435/78; wall max 462431 ms; provider input/output tokens 2384992/224838 (cached input 280320); schema tokens 277993. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

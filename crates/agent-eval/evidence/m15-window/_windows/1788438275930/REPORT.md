# M15 development window — FAILED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 6 | 12 | 73311 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 9 | 17 | 98488 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 11 | 20 | 116684 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 11 | 28 | 124821 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 21 | 93531 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 7 | 14 | 86918 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 11 | 21 | 109145 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 10 | 22 | 164429 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 42 | 82 | 406966 | pass |
| retry_policy_dev normal r2 | pass | pass | failed | n/a | healthy | 48 | 94 | 479908 | fail |
| retry_policy_dev resume r1 | pass | pass | failed | failed | healthy | 6 | 13 | 75591 | fail |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 20 | 50 | 212237 | pass |

Summary: pass 10/12; NOT_RUN 0/12; behavior pass 12/12; closures 2/12.

Efficiency facts: rounds total/max 191/48; tool calls total/max 394/94; wall max 479908 ms; provider input/output tokens 1899276/72102 (cached input 171520); schema tokens 202210. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

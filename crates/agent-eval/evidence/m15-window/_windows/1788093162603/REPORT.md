# M15 development window — FAILED

Schema `m15-window.v1`. Generated mechanically from immutable cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 17 | 120591 | pass |
| retry_diag_dev normal r2 | fail | pass | active | n/a | healthy | 6 | 12 | 84611 | fail |
| retry_diag_dev resume r1 | fail | pass | active | restored_and_continued | healthy | 11 | 24 | 140252 | fail |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 11 | 22 | 120672 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 11 | 24 | 107442 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 6 | 14 | 69200 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 30 | 44 | 215221 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 12 | 24 | 186279 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 11 | 25 | 156650 | pass |
| retry_policy_dev normal r2 | pass | pass | failed | n/a | healthy | 48 | 103 | 409132 | fail |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 31 | 61 | 324403 | pass |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 41 | 82 | 509357 | pass |

Summary: pass 9/12; NOT_RUN 0/12; behavior pass 10/12; closures 3/12.

Efficiency facts: rounds total/max 228/48; tool calls total/max 452/103; wall max 509357 ms; provider input/output tokens 2575264/91164 (cached input 200704); schema tokens 239146. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

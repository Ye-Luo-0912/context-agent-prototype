# M15 development window — PASS

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 17 | 24 | 206500 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 7 | 11 | 209564 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 13 | 26 | 149911 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 17 | 30 | 382008 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 8 | 20 | 220242 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 7 | 14 | 118369 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 16 | 34 | 433062 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 12 | 27 | 165815 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 33 | 62 | 562563 | pass |
| retry_policy_dev normal r2 | pass | pass | completed | n/a | healthy | 20 | 40 | 250329 | pass |
| retry_policy_dev resume r1 | pass | pass | completed | restored_and_continued | healthy | 34 | 75 | 458460 | pass |
| retry_policy_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 28 | 62 | 417801 | pass |

Summary: pass 12/12; NOT_RUN 0/12; behavior pass 12/12; closures 3/12.

Efficiency facts: rounds total/max 212/34; tool calls total/max 425/75; wall max 562563 ms; provider input/output tokens 2178893/81209 (cached input 112640); schema tokens 224112. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

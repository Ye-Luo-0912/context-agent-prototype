# M15 development window — CENSORED

Schema `m15-window.v2`. Generated mechanically from content-addressed cell bundles.

| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |
| retry_diag_dev normal r1 | pass | pass | active | n/a | healthy | 7 | 12 | 96835 | pass |
| retry_diag_dev normal r2 | pass | pass | active | n/a | healthy | 10 | 18 | 131591 | pass |
| retry_diag_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 26 | 41 | 224052 | pass |
| retry_diag_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 13 | 24 | 148475 | pass |
| retry_migrate_dev normal r1 | pass | pass | active | n/a | healthy | 10 | 18 | 98246 | pass |
| retry_migrate_dev normal r2 | pass | pass | active | n/a | healthy | 13 | 28 | 137245 | pass |
| retry_migrate_dev resume r1 | pass | pass | active | restored_and_continued | healthy | 14 | 26 | 126499 | pass |
| retry_migrate_dev resume r2 | pass | pass | active | restored_and_continued | healthy | 13 | 24 | 132258 | pass |
| retry_policy_dev normal r1 | pass | pass | completed | n/a | healthy | 44 | 91 | 343673 | pass |
| retry_policy_dev normal r2 | not_run | pass | failed | n/a | transport_failed | 1 | 0 | 51712 | not_run |
| retry_policy_dev resume r1 | not_run | pass | failed | restored_and_continued | transport_failed | 19 | 42 | 320797 | not_run |
| retry_policy_dev resume r2 | pass | pass | completed | restored_and_continued | healthy | 17 | 36 | 190567 | pass |

Summary: pass 10/12; NOT_RUN 2/12; behavior pass 10/12; closures 2/12.

Efficiency facts: rounds total/max 187/44; tool calls total/max 360/91; wall max 343673 ms; provider input/output tokens 1817301/63281 (cached input 142848); schema tokens 196633. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.

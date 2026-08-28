# M15 legacy-window forensic audit — 2026-08-28

Verdict: **INVALID AS FORMAL EVIDENCE. M15 remains open.**

This directory retains three immutable `retry-pilot-cell-v2` attempts. They
are useful for finding evaluator and serving problems, but they do not satisfy
the corrected M15 evidence contract and must not be used for promotion,
serving selection or cross-window causal claims.

## Why the attempts are invalid

1. The evaluator projected missing `task.complete` as a Runtime failure even
   though M15 V1 explicitly treats closure as report-only.
2. Every manifest used the `retry_policy_dev` fixture id and digest, including
   diagnosis and migration cells. The pack identity therefore cannot prove
   which frozen fixture was executed.
3. Provider health was inferred from error-message substrings rather than a
   typed Runtime failure source.
4. `max_output_tokens` was reported as relay transport loss. It is an
   incomplete model response caused by an output limit and is a cell failure,
   not proof of provider unavailability.
5. The aggregate Markdown was hand-maintained and its pass, behavior, closure
   and failure counts do not consistently match the retained cell facts.

Any one of these faults prevents a formal verdict; together they invalidate
all three windows.

## Raw observations under the repaired taxonomy

These counts are forensic observations only. “Eligible behavior” means the
recorded behavior/diff/continuation facts would satisfy the closure-free M15
profile if identity and failure evidence were otherwise valid.

| attempt | serving/surface | eligible behavior | closure observed | execution anomalies | evidence status |
| --- | --- | ---: | ---: | --- | --- |
| 1 | Luna / pre-v5 | 9/12 | 1/12 | none recorded | invalid v2 identity/projection |
| 2 | Luna / v5 | 7/12 | 4/12 | one ~300 s watchdog; one round-budget failure | invalid; a repaired window would be censored by the watchdog |
| 3 | deepseek-v4-pro / relay | 2/12 observed pass | 5/12 | six `max_output_tokens` outcomes | invalid; output-limit failures are not transport outages |

The observations do not prove that v5 caused closure improvement, that Luna
has a stable ceiling, or that the relay lost its session. The attempts were
not valid paired experiments, and the third attempt did not record the typed
distinction needed for a provider-health claim.

## Repaired evidence path

New M15 cells use `retry-pilot-cell-v3` with:

- the actual pack id and pack-specific digest;
- persisted acceptance profile and PASS/FAIL/NOT_RUN verdict;
- independent restore, exact-tuple, continuation, turn and task-completion
  facts;
- typed provider transport, model output-limit, budget, Runtime and harness
  failures; and
- a `_windows/<timestamp>/manifest.json` naming the exact 12 cells, from which
  `REPORT.md` is generated and can be regenerated mechanically.

Only a complete, clean-tree v3 window on one pinned serving may replace this
forensic report as formal M15 evidence.

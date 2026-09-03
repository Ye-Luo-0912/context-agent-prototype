# Successor-candidate rerun diagnosis (2026-09-03)

Post-window analysis of the authorized whole-window rerun of the eighth v4
window (`_windows/1788466134988`, frozen source `1651354`, PinAI
`gpt-5.6-luna`, Responses). The rerun is a valid FAIL 6/12, 0 NOT_RUN; every
fact below is read from the immutable cell streams.

## Diag overflow edge — stochastic trap, 1/4

`normal r2`, `resume r1` and `resume r2` all wrote the `checked_shl` trap
(guards shift counts, not bits shifted out) and failed the single gating
assertion "shift corrected and overflow-safe". `normal r1` wrote the
`checked_mul` shape and passed overall (its residual assertion miss is the
known non-gating needle-text tolerance). Across every banked window the
passing diag cells are exactly the `checked_mul`-shape cells; the fixture,
oracle and needles are unchanged. Confirmed again: a per-cell stochastic
solver limitation, not a harness defect.

## Policy completion-gate tails — dominant surface, 1/4

`normal r1` (7 refusals), `normal r2` (4) and `resume r1` (14) all exhausted
the 48/55-round budget with behavior pass and diff pass; zero `task.complete`
calls were accepted in any of the four policy cells. `resume r2` closed
ordinary-final after 1 refusal in 39 rounds. The refusals re-state the
compound completion-gate grounds (verification currency + acceptance
coverage + open loops + failed-command rows); the repaired source surfaces
the completion_repair stage, and the model still churns verification and
patch attempts instead of converging the blocker set. Stochastic: the same
source/serving closed `normal r1` in 44 rounds in the censored first run.

## Cross-window assessment

Nine v4 windows now span three sources and two servings. The infrastructure
is exonerated everywhere (provider healthy in all cells of both runs; the
only transport failure class was upstream 503, which censors rather than
fails). The recurring failure surfaces are both per-cell model behavior:
the diag `checked_shl` trap and the policy completion-gate tail. Their
per-window rates vary widely on identical source+serving (diag 1/4..4/4,
policy 1/4..4/4), which is direct evidence that serving/model quality, not
repository code, is the current gate on M15.

# Completion-gate convergence candidate M15 v4 window failure diagnosis (2026-09-03)

Post-window evidence analysis of the seventh formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788438275930`, predeclared
before its run on 2026-09-03; exact clean source identity recorded at launch,
`git_head a6dc33e`, source tree digest `3e2a212d…`), following the
M15_ACCEPTANCE §5 return-to-diagnosis from its valid FAIL (10/12). Serving
tuple: PinAI (`https://api.pinaic.com/v1`), `gpt-5.6-luna`, Responses, explicit
protocol, 128,000-token context, 4,096 max output tokens. All facts are read
from the immutable per-cell event streams, per-cell `summary.json`,
`dimensions.json` and the harness `verify.json` records; no new live run
produced them.

Verdict shape: **10/12 pass, 0 NOT_RUN** — the mechanical report at
`_windows/1788438275930/REPORT.md`. Behavior pass 12/12 and allowed-diff pass
12/12; provider healthy in every cell; closures 2/12. Migrate 4/4
(7–11 rounds, clean continuation everywhere resumed); diag 4/4 (6–11 rounds,
all `active`); policy 2/4. Rounds 191 total / max 48; tool calls 394 / max 94;
wall max 479,908 ms; provider input 1,899,276 / output 72,102 (cached 171,520);
schema tokens 202,210.

The candidate materially improved the diag fixture that dominated the previous
window: all four `retry_diag_dev` cells now carry `closure active` and converge
(6–11 rounds each), versus the operator-only completion loop before. The two
remaining failures are both in `retry_policy_dev` and are **two distinct
mechanisms, neither of which the completion-gate terminal escalation covered**.
Both failing cells finished functionally-correct workspaces (the injected
harness oracle tests pass), consistent with the standing completion-gate-tail
characterization, but the root causes differ.

## 1. `retry_policy_dev` normal r2 — persistent execution-debt, budget exhaustion

| fact | value |
| --- | --- |
| rounds | 48 (phase one), budget exhausted |
| `task.complete` | refused 6/6 |
| oracle | behavior pass, allowed-diff pass, workspace self-check pass |
| failure class | `round_budget` (typed, non-retryable) |
| authority evidence | `m15-retry_policy_dev-normal/r2-attempt13/dynamic/` |

Behavior facts from the immutable streams:

- The final workspace is functionally correct (behavioral oracle pass, allowed
  diff pass) — a `retry_policy_dev` run of the harness passes; the residual
  obligation ledger still held `failed_commands remaining: 1` from a single
  failed `shell.exec` (exit≠0, `tool_failure_classes.process_exit = 1`).
- The six refused `task.complete` calls cite two blocker shapes only:
  `execution_debt` (`failed_commands remaining: 1`) and, at the tail,
  `task_progress` (`next_action_pending`). No `operator_required` / no-resolver
  stage appears, so the terminal escalation
  (`COMPLETION_REPAIR_TERMINAL_REFUSALS = 3`, `terminal_surface:
  "ordinary_final"`) **never fired** — the event stream carries zero
  `ordinary_final` / `terminal_surface` markers.
- The model spent the full 48-round budget trying to clear the failed-command
  debt and/or satisfy the pending-next-action projection, re-proposing
  `task.complete` as the projected work changed (basis moved), and exhausted
  the budget without achieving durable closure.

Mechanism: the terminal escalation is deliberately scoped to
`operator_required` / no-safe-model-owned-resolver stages. Here the blocker was
a **resolvable-looking execution-debt obligation** (one failed command), which
the mechanism intentionally excludes — so the candidate neither converted the
tail nor admitted a converging end; the model churned against persistent debt
until `round_budget`. This is a genuinely different residual than the previous
window's operator-only diag loop and is not covered by this candidate.

## 2. `retry_policy_dev` resume r1 — storage lock contention at checkpoint load

| fact | value |
| --- | --- |
| rounds | 6 (phase one) |
| provider | healthy |
| oracle (on disk) | harness verifier `cargo test` exit 0, workspace correct |
| failure class | `runtime` (typed, non-retryable) |
| authority evidence | `m15-retry_policy_dev-resume/r1-attempt13/dynamic/` |

Behavior facts:

- The resume never reached a completion decision. At checkpoint-artifact load
  the harness failed to re-acquire the exclusive lock on the workspace effect
  journal and bailed: `checkpoint artifact load failed: storage error: lock
  workspace effect journal …\\.focus-agent\\authority\\workspace-effects.jsonl
  exclusively: lock acquisition failed because the operation would block (still
  contested after 20 retries)`.
- `continuation: failed`, `continued: false`. The on-disk workspace is
  functionally correct; the failure is entirely in restore-time storage
  coordination, not model behavior or the completion gate.

Mechanism: a stale or concurrently-held exclusive lock on
`workspace-effects.jsonl` blocked checkpoint restore (a Windows file-lock /
ownership gap at the cell boundary or a leftover from a prior attempt). It is
an infrastructure defect, not a completion-gate tail, and is unrelated to the
candidate.

## 3. Harness / provider surface noise (non-censorious)

Across the per-cell retry streams the known fleet of host/provider surface
issues recurred transiently — non-retryable `max_output_tokens` transport
errors, `malformed-tool-call` EOF parses, one `malformed-event` runaway stream
(16,385/16,384 chunks — the known first-occurrence delta stream), one
`cell stalled waiting for runtime events`, and the resume lock failure above.
None censored the window: each cell produced a final decision-grade attempt and
the mechanical report counts 0 NOT_RUN. They are consistent with already-known
P1-class harness/provider defects and do not change the FAIL verdict.

## Conclusion

The window is a **valid FAIL**: M15_ACCEPTANCE §5 rejects the completion-gate
convergence candidate on this source; the window is not rerun. The candidate
closed the diag tail (diag 4/4) but did not cover the two residual policy-cell
failure classes: (1) a resolvable-looking persistent execution-debt blocker on
which the intended ordinary-final terminal stage is deliberately not offered,
driving a 48-round budget exhaustion; and (2) a harness storage lock-contention
failure at resume restore that is infrastructure, not model behavior. Candidate
selection returns to diagnosis; the next bounded M15 candidate is an operator
decision bounded by the frozen route (no Context/GC retune, no protocol
weakening, no round stop, no prompt pressure).
# Completion-gate convergence candidate M15 v4 window failure diagnosis (2026-09-03)

Post-window evidence analysis of the seventh formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788438275930`, predeclared
before its run on 2026-09-03; exact clean source identity recorded at launch,
`git_head a6dc33e`, source tree digest `3e2a212d…`), following the
M15\_ACCEPTANCE §5 return-to-diagnosis from its valid FAIL (10/12). Serving
tuple: PinAI (`https://api.pinaic.com/v1`), `gpt-5.6-luna`, Responses, explicit
protocol, 128,000-token context, 4,096 max output tokens. All facts are read
from the immutable per-cell event streams, per-cell `summary.json`,
`dimensions.json` and the harness `verify.json` records; no new live run
produced them.

## Successor-source implementation re-audit (2026-09-03; no evidence rewrite)

A code-and-event re-audit after this report was recorded corrected the causal
attribution in §1 while preserving the window and its 10/12 FAIL verdict. The
failed formatting check was not the surviving row: its failed operation at
seq 382/385 and successful rerun at seq 410/413 have the same tool name and
the same non-empty Runtime `argument_digest` (`6359474a…`), so the existing
`same_operation` success path retires that `NonDeterministic` row. The earlier
`fs.read src/job.rs` miss at seq 56–58 is the row that survived. It was a
trusted `PathNotFound` negative observation, but `freshness.rs` also inserted
every failed read into `failed_commands` even when Runtime had not rooted that
speculative path in the task. Later model actions explicitly try to clear the
“stale `src/job.rs` observation” (for example seq 756 onward), independently
confirming which blocker it continued to see.

The successor repair therefore does not auto-clear a command, infer success
from prose, or weaken rooted resource debt. It makes one already-attributed
case internally consistent: `Observe/Search + PathNotFound + exact host target
match + unrooted by Runtime` remains a revision-bound negative fact but never
becomes task-completion debt. Rooted, unattributed, target-mismatched and other
failure classes remain conservative. Deterministic regressions cover all of
those branches.

The §2 source audit also found why the lock could outlive shutdown: the
detached model future cloned the complete `RuntimeServices`, which includes
tool/artifact `Workspace` handles. A provider that observed cancellation late
could therefore retain the exclusive workspace-effect journal after the actor
had emitted `RunCompleted`. The successor implementation captures only the
`ModelTransport` in that future. Its regression keeps a cancellation-ignoring
model future alive, shuts down the actor, and successfully reopens the same
workspace (and its exclusive journal lock) before releasing the model.

Classification clarification: the immutable cell's `dimensions.json` records
`runtime_error_class: "runtime"`, `verdict: "fail"` and 0 NOT_RUN for the
window. The phrases “harness storage coordination” and “harness failure” in
the original interpretation below describe that the eval host exercised a
fresh Runtime restore; they do **not** denote the frozen
`harness_setup`/`harness_watchdog` class. The failure occurred inside the
product Runtime/workspace lifecycle and therefore remains consistent with the
mechanical valid-FAIL verdict. This clarification changes no cell artifact or
window report.

These are successor-source repairs only. No formal preflight or live cell was
run, no historical artifact was regenerated, and this valid FAIL remains the
decision for source `a6dc33e`. The original post-window interpretation is kept
below for audit history; this addendum governs the corrected root-cause
attribution.

Verdict shape: **10/12 pass, 0 NOT\_RUN** — the mechanical report at
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

| fact               | value                                                       |
| ------------------ | ----------------------------------------------------------- |
| rounds             | 48 (phase one), budget exhausted                            |
| `task.complete`    | refused 6/6                                                 |
| oracle             | behavior pass, allowed-diff pass, workspace self-check pass |
| failure class      | `round_budget` (typed, non-retryable)                       |
| authority evidence | `m15-retry_policy_dev-normal/r2-attempt13/dynamic/`         |

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

### Root-cause chain (normal r2), read from the immutable stream + `<cmd>tool.rs</cmd>`,

`execution/state.rs`, `execution/freshness.rs`

1. In round 20 the model loaded `shell.exec` and ran the formatting check
   `cargo fmt --all -- --check` (seq 383–385), which exited **1** because
   formatting was pending. `failure_class=process_exit`, and the output carried
   no `resolution_scope_key` / `resolution_fingerprint` (it is not a typed
   `verify.run` recipe).
2. `ToolFailureDomain` mapping: `ProcessExit → NonDeterministic`
   (`agent-contracts/src/tool.rs:1227`). `failure_blocker_identity` returns
   `None` when both `scope_key` and `precondition` are empty, so `push_failure`
   records the command as a **NonDeterministic** failed-command row.
3. The model then resolved the literal formatting need: `cargo fmt --all`
   (success), re-ran the identical check `cargo fmt --all -- --check`
   (success, **same** **`argument_digest`** **`6359474a…`** as the failed run — verified
   from `operation_accepted` seq 382 vs 410), and `cargo clippy … -D warnings`
   (success). The workspace is functionally complete (all four injected oracle
   unit tests + the integration test pass).
4. Retirement of a NonDeterministic failed command has **one** path: the
   success-path `retain` in `freshness.rs:189` via `same_operation`
   (tool\_name + non-empty equal `argument_digest`). The ledger predicate
   `resolve_failure_blockers` → `failure_resolution` returns
   `FailureResolution::Keep` for `NonDeterministic` unconditionally
   (`state.rs:2227`). The producers pass the runtime `argument_digest`
   verbatim (`actor/tools.rs:1545`) so the digest cannot be missing on that
   side.
5. Despite the exact-command successful re-run, **all six** **`task.complete`
   refusals still cite** **`failed_commands remaining: 1`** (seq 561/633/725/775/
   839/890). The debt was never retired, so the completion gate was
   permanently unsatisfiable; the model worked to clear it across the
   remaining rounds and exhausted the 48-round budget (`round_budget`,
   non-retryable).

Defect class (concrete): **a "check-then-fix" command that exits non-zero as
its normal transient signal (`fmt --check`,** **`clippy --check`, first-run
tests) creates durable NonDeterministic completion debt that this code base
intentionally keeps irreconcilable by the ledger predicate and, in this run,
was not retired by the exact-command re-run** — so a functionally-correct,
oracle-green workspace can never satisfy durable closure. The intent of the
design (do not let a failing command silently "self-heal" on paper) is fine;
the gap is that a check command whose *purpose* is to report non-conformance,
then pass after the conforming edit, is indistinguishable here from a command
that genuinely keeps failing. The ordinary-final terminal stage this window
tested does not cover this, because the blocker is model-resolvable by design.

## 2. `retry_policy_dev` resume r1 — storage lock contention at checkpoint load

| fact               | value                                                   |
| ------------------ | ------------------------------------------------------- |
| rounds             | 6 (phase one)                                           |
| provider           | healthy                                                 |
| oracle (on disk)   | harness verifier `cargo test` exit 0, workspace correct |
| failure class      | `runtime` (typed, non-retryable)                        |
| authority evidence | `m15-retry_policy_dev-resume/r1-attempt13/dynamic/`     |

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

Mechanism: the resume did reach the workspace and performed real work (the
immutable stream shows 6 rounds and 13 tools; at round 4 it issues the full
`edit.patch` implementing the retry policy), so this is **not** a completion-gate
or model tail. The cell is invalidated by harness restore-time **storage
coordination**: re-acquiring the exclusive lock on the workspace effect journal
(`.focus-agent/authority/workspace-effects.jsonl`) bailed — `lock acquisition
failed because the operation would block (still contested after 20 retries)` —
and the terminal `turn_cancelled` + `continuation: failed` follow. A stale or
concurrently-held Windows exclusive lock from a previous phase/process (the
prior cells share the `.tmp…` workspace dirs) was not released on teardown, so
this resume hard-failed at checkpoint artifact load. It is an infrastructure
defect, not model behavior, and unrelated to the candidate.

## 3. Harness / provider surface noise (non-censorious)

Across the per-cell retry streams the known fleet of host/provider surface
issues recurred transiently — non-retryable `max_output_tokens` transport
errors, `malformed-tool-call` EOF parses, one `malformed-event` runaway stream
(16,385/16,384 chunks — the known first-occurrence delta stream), one
`cell stalled waiting for runtime events`, and the resume lock failure above.
None censored the window: each cell produced a final decision-grade attempt and
the mechanical report counts 0 NOT\_RUN. They are consistent with already-known
P1-class harness/provider defects and do not change the FAIL verdict.

## Conclusion

The window is a **valid FAIL**: M15\_ACCEPTANCE §5 rejects the completion-gate
convergence candidate on this source; the window is not rerun. The candidate
closed the diag tail (diag 4/4) but did not cover the two residual policy-cell
failure classes: (1) a resolvable-looking persistent execution-debt blocker on
which the intended ordinary-final terminal stage is deliberately not offered,
driving a 48-round budget exhaustion; and (2) a harness storage lock-contention
failure at resume restore that is infrastructure, not model behavior. Candidate
selection returns to diagnosis; the next bounded M15 candidate is an operator
decision bounded by the frozen route (no Context/GC retune, no protocol
weakening, no round stop, no prompt pressure).

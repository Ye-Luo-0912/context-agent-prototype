# JSON-hardened M15 v4 window failure diagnosis (2026-08-31)

Post-window evidence analysis of the fourth formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788115951355`, clean
HEAD `784d7aa`), following the M15_ACCEPTANCE §5 rule 4 return-to-diagnosis
from its valid FAIL (10/12). Serving tuple: localhost OpenCode relay
(`http://127.0.0.1:8787/v1`), `deepseek-v4-flash`, Responses, 32,768 max
output tokens. All facts are read from the immutable per-cell event
streams, per-cell `summary.json` and the harness `verify.json` records; no
new live run produced them.

Verdict shape: 10/12 pass, 0 NOT_RUN, behavior pass 12/12, provider
healthy in every cell. Diag 4/4, migrate 4/4, policy 2/4. The two
failures are `retry_policy_dev-normal` r1 and r2, both
`closure=error` / "phase one failed: tool round budget exhausted after
48 rounds". No `malformed-tool-call` outcome occurs anywhere in the
window; the JSON-hardening + bounded-retry fix (commit `41f06ad`)
removed its target failure mode. The new retry path was exercised twice
(`retry_migrate_dev-resume` r2, `retry_policy_dev-resume` r1), both
ending passed; one `model_used` event records 2 attempts / 1 retry.

## 1. The two failing cells

| cell | rounds | error detail |
| --- | --- | ---: |
| `retry_policy_dev-normal/r1-attempt10` | 48 | `phase one failed: tool round budget exhausted after 48 rounds`; `task.complete` refused 3/3 |
| `retry_policy_dev-normal/r2-attempt10` | 48 | same error; `task.complete` refused 5/5 |

Every `task.complete` refusal names the same completion-gate blockers:
`trusted verification is not current`, `N acceptance criterion/criteria
lack current coverage`, `M unresolved failed command(s) remain` (M = 1–2;
r2 also adds explicit open loops and a concrete next action on its last
attempt). The model never got a refusal it could act on: its final
verification runs were green and its workspace satisfied the directive.

## 2. The model completed the functional task

The frozen directive (`crates/agent-eval/src/long_task.rs` `DIRECTIVE`)
asks for a configurable bounded exponential retry policy: retry only
transient errors, permanent errors return immediately, `max_attempts`
includes the first call, delay growth saturates at `max_delay_ms`, keep
the public `run_job` signature, add unit/integration coverage, update the
README, run the checks, review the diff and report.

The final workspace (r1) implements every requirement and the runtime
checks agree:

- `verify.run rust.workspace` and `verify.run jobrunner.exact` were green
  four consecutive times (exit=0, 15 tests passed, `cargo fmt --check`
  clean) — the last fully green verify pair is immediately before the
  final `task.complete` attempt;
- the model's own unit + integration suite covers saturation at
  `max_delay_ms`, overflow safety at extreme indices, `max_attempts`
  including the first call, transient-only retry;
- the final three `task.complete` refusals in r1 follow three successful
  `shell.exec` runs (`cargo fmt --check`, `cargo test`,
  `cargo clippy --all-targets -- -D warnings`).

## 3. Why the completion gate stayed shut

The cell profile is `AcceptanceProfile::ClosureRequired` with
`TaskCompletionPolicy::EvidenceRequired` and one declared acceptance
criterion bound to the verify recipe. Completion requires, at the final
moment: verification current on the exact world basis (no may-mutate
observation since the last trusted PASS), every criterion covered by a
current receipt, and zero unresolved failed commands. Three facts keep
the gate shut:

1. **Verification went stale after the final verify.** The last
   `verify.run` pair passed, then the model ran three more `shell.exec`
   commands (`cargo test`, `cargo fmt --check`, `cargo clippy`). Any
   may-mutate observation bumps the workspace revision, expiring earlier
   verification facts; the subsequent `task.complete` was refused with
   `VerificationNotCurrent` + `AcceptanceUncovered{1}`. The model did not
   re-verify after its last shell commands.
2. **Early fail-closed tool calls left permanent failed-command rows.**
   In round 1–2 (surface revision 6) the model called `shell.exec` before
   loading it through `capability.manage` ("tool 'shell.exec' was not
   exposed on model surface revision 6"), plus `fs_mkdir`/`shell_exec`
   wire-name variants and an `fs.write` to a missing parent path. These
   non-deterministic failures are recorded in the execution
   `failed_commands` ledger; a row leaves that list only through an exact
   same-operation success or the typed obligation matcher, and
   `ToolFailureDomain::NonDeterministic` failures are never resolved by a
   later success. The refusals cited 1–2 such rows persistently.
3. **The model could not interpret the refusals.** It saw green
   verification and a completed diff, so it looped through
   `capability.manage` loads, repeated `verify.run` reruns and repeated
   `task.complete` proposals for the final ~10 rounds, exhausting the
   48-round budget without ever re-verifying as its last action.

## 4. Oracle static checks bind implementation detail, not behavior

The pack's hidden checks (`HIDDEN_CHECKS` in `crates/agent-eval/src/
long_task.rs`) are needle predicates over final file text. The r1
workspace fails three of six, although the file contents satisfy the
directive:

| check | needle | final workspace |
| --- | --- | --- |
| `src/error.rs` transient classification | `=> true` and `=> false` | uses `matches!(self, RetryError::Transient(_))` |
| `src/lib.rs` retry loop bounds on `max_attempts` | `1..` and `max_attempts` | uses `remaining > 1` / `remaining -= 1` |
| `src/lib.rs` delay growth saturates | `.min(` in `lib.rs` | saturation lives in `config.rs::delay_before_retry` |

These `passed=false` rows are reported per check in `verify.json` and do
not by themselves close the completion gate (the receipt binds the verify
recipe), but they show the oracle is not robust to equivalently correct
implementations and are recorded here as a fixture-diagnostic weakness.

## 5. Contrast with the passing cells

The same fixture's resume r1 and r2 pass (`closure=completed`), and
resume r2 completes on its first `task.complete` after 21 rounds with one
retry. Diag and migrate pass 8/8. The difference is behavioral, not
systemic: the passing cells load tools before first use and keep their
last mutation/verify ordering such that the final trusted PASS is current
when `task.complete` is proposed. The failing normal cells had early
fail-closed tool-name attempts (permanent ledger rows) and ran shell
commands after the last verify. The same fixture passed its normal cells
in the 16,384-tuple window, so neither the fixture nor the serving makes
completion impossible — the failure is model behavior variance on this
surface.

## 6. Verdict

Per M15_ACCEPTANCE §5 rule 4 the valid FAIL (10/12, 0 NOT_RUN) rejects
the candidate and returns to diagnosis; the window is not rerun. The
`41f06ad` fix eliminated its target failure mode (zero malformed
tool-call arguments across 12 cells, retry path exercised twice with both
cells passing) and exposed a completion-gate compliance failure on
`retry_policy_dev-normal`. Candidate routes for a future preflight +
window (each requires an exact-source preflight and a fresh predeclaration
per M15_ACCEPTANCE §7): (a) a harness-visible completion harness
adjustment — e.g. excluding fail-closed "not exposed" rejections from the
permanent failed-command ledger, or surfacing a clearer "re-verify after
your last command" signal — with deterministic gates before any live run;
(b) a prompt-layer note that the final action must be `verify.run` with
no further commands or edits after it; (c) another user decision. M15
remains open.
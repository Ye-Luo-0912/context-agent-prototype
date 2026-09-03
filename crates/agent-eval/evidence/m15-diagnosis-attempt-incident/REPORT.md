# Attempt-incident admission candidate M15 v4 window failure diagnosis (2026-09-03)

Post-window evidence analysis of the sixth formal v4 window
(`crates/agent-eval/evidence/m15-window/_windows/1788402676712`, predeclared
before its run on 2026-09-03; per-cell source identity `git_head
38d458e73882136972a12a19da6c95c1cdfe40a2`, source tree digest
`0cecc539d3dcc2c791a0ad15f077ddd287304ea3bd7eda4487bc229a504af5d5`, clean
tree), following the M15_ACCEPTANCE §5 return-to-diagnosis from its valid
FAIL (10/12). Serving tuple: PinAI (`https://api.pinaic.com/v1`),
`gpt-5.6-luna`, Responses, explicit protocol, 128,000-token context, 4,096
max output tokens. All facts are read from the immutable per-cell event
streams, per-cell `summary.json`, `dimensions.json` and the harness
`verify.json` records; no new live run produced them.

Verdict shape: **10/12 pass, 0 NOT_RUN** — the mechanical report at
`_windows/1788402676712/REPORT.md`. Behavior pass 12/12 and allowed-diff
pass 12/12; provider healthy in every cell; closures 3/12 (all three
`retry_policy_dev` completed cells). Migrate 4/4 (7–19 rounds, clean),
diag 3/4, policy 3/4. Rounds 274 total / max 54; tool calls 499 / max 93;
wall max 458,429 ms; provider input 2,750,601 / output 81,138 (cached
293,888); schema tokens 290,539.

The window is a valid FAIL: two cells exhausted the 48-round tool budget
instead of closing. Both failing cells finished functionally-correct
workspaces (the injected harness oracle tests pass in both), so this
window's failures are **completion-gate tails with static marker-check
misses**, not behavioral or transport defects.

## 1. `retry_diag_dev` normal r2 — correct fix, marker miss, completion fixation

| fact | value |
| --- | --- |
| rounds | 48 (phase one), budget exhausted |
| `task.complete` | refused 18/18 |
| oracle | `cargo test --test m15_diag_oracle`: 3/3 pass; self-check pass |
| workspace hash | `d515792b…` (7 files) |
| needle assertion | `src/backoff.rs` "shift corrected and overflow-safe" → **false** |
| failure class | `round_budget` (typed, non-retryable) |

Behavior facts from the immutable streams:

- The model wrote a **functionally correct** `next_delay`: `shift =
  attempt.saturating_sub(1).min(63)`, `factor = 1u64 << shift`, then
  `checked_mul(factor).unwrap_or(u64::MAX)` capped by `min(max_delay_ms)`.
  The injected behavioral oracle (3 tests: first retry = base, doubling then
  saturation, public signature) passes 3/3 on the final workspace, and the
  self-check passes. This is the first failing diag cell that does **not**
  carry the `checked_shl` behavioral trap from the v3 windows and the
  `43e1033` window.
- The static marker check `shift corrected and overflow-safe` still requires
  the body to contain `u128` or `leading_zeros` (see `m15_pack.rs`
  `DIAG_CHECKS`); the written `checked_mul` + `min(63)` shape contains
  neither literal, so the PackCheck stays false. This is a needle-shape
  miss, not a functional failure — the oracle and self-check both pass.
- The cell then spent the budget in a completion-gate loop: 18
  `task.complete` proposals, every one refused with the persistent blocker
  set `verification_not_current` + `acceptance_undeclared` +
  `operator_closure_only` (the diag fixture declares no acceptance domain;
  its `acceptance_declaration_revision` is null), later joined by
  `next_action_pending`. The refusal repeatedly states "task policy permits
  durable closure only by an explicit operator"; Runtime's
  `completion-repair.v1` stage resolves to `operator_required`. The model
  kept re-proposing `task.complete` and re-running `capability.manage` (10),
  `task.manage` (4) and `verify.run` (7, one failed) instead of ending
  ordinary-final like the passing diag cells (closures `active`), and the
  phase-one 48-round budget expired.

Conclusion: the diag fix is now behaviorally solved on this serving; the
residual miss is the static marker wording, and the cell fails because the
model fixated on a closure surface the fixture structure cannot grant
(operator-only durable closure, no declared acceptance) and exhausted the
budget. Not a harness, transport or oracle defect.

## 2. `retry_policy_dev` resume r1 — functionally correct, three marker misses, tail churn

| fact | value |
| --- | --- |
| rounds | 6 phase one + 48 phase two, budget exhausted |
| continuation | restored, exact tuple matched, continued |
| `task.complete` | refused 12/12 |
| oracle | `cargo test --test retry_policy_oracle`: 7/7 pass; self-check pass |
| needle assertions | `src/error.rs` "transient classification implemented" → false; `src/lib.rs` "retry loop bounds on max_attempts" → false; `src/lib.rs` "delay growth saturates at max_delay_ms" → false |
| failure class | `round_budget` (typed, non-retryable) |

Behavior facts from the immutable streams:

- The written workspace is behaviorally correct: `is_transient()` via
  `matches!(self, Self::Transient(_))`, `run_job` loops `0..max_attempts`
  with `attempt + 1 < max_attempts`, delays via
  `base.saturating_mul(2u64.saturating_pow(n)).min(max)`. The injected
  functional oracle passes 7/7 and the self-check passes.
- The three PackChecks are static-shape predicates that the written shape
  does not satisfy: the transient classifier uses a `matches!` arm instead
  of `=> true` / `=> false` literals, the loop idiom is `0..` + explicit
  bound rather than `1..`, and the `.min(` saturation lives in `config.rs`
  (`delay_for_retry`) rather than in `src/lib.rs`. All three are marker
  misses on a functionally correct implementation, not behavior failures.
- The cell then churned against the completion gate for the full phase-two
  budget: 12 `task.complete` refusals whose reasons progress from
  "trusted verification is not current; 1 acceptance criterion/criteria
  lack current coverage" through "2 explicit open loop(s) remain; a
  concrete next action remains" to a bare "a concrete next action remains".
  The heavy tail (`git.status` 8, `git.diff` 6, `fs.read` 25, `verify.run`
  7, `edit.patch` 4 with 2 failed, `task.manage` 11) re-staled
  verification and never cleared `next_action`/open loops before the
  48-round phase-two budget expired.
- Notably, no refusal names an unresolved failed-command row: the
  attempt-incident admission surface worked as designed for off-surface
  refusals (`task.manage` 1 invalid-request and the `no_exact_match` /
  `stale_revision` edit refusals did not open debt rows — obligations
  opened 2 / resolved 2, negative facts recorded 1). The unclosed surface
  is verification currentness plus acceptance coverage plus the open
  `next_action`/loops, not failed-command debt from off-surface calls.

Conclusion: a policy completion-gate tail (the same family as the
`784d7aa`/`43e1033` tails) on a functionally-correct workspace, now without
the failed-command-debt amplifier the attempt-incident slice removed. Three
static marker checks miss; behavior, diff, provider and continuation are
clean. Not a harness, transport or oracle defect.

## 3. What improved versus the prior windows

- Behavior pass 12/12 and allowed-diff pass 12/12 — the first v4 window in
  which every cell's injected functional oracle is green.
- Diag: the `checked_shl` overflow trap did not recur; the failing diag cell
  wrote a behaviorally correct fix and missed only the static marker text.
- Policy: 3/4 cells closed with `task.complete` accepted (normal r1 36
  rounds, normal r2 25 rounds, resume r2 36 rounds), including one resume
  cell; the failing cell's refusals never cite failed-command debt, so the
  2026-08-31 P1 admission guarantee (off-surface refusals stay visible but
  create no completion debt) held on the live surface.
- No malformed tool-call, no bounded-framer chunk-cap event, no provider
  transport interruption in any cell.

## 4. Cross-class assessment

The two failures are the same class — completion-gate compliance tails on
functionally correct workspaces — each with static marker-check misses at
the PackCheck level (the exact `u128`/`leading_zeros` and armed-arm /
loop-idiom / `.min`-location literals the frozen fixture binds). Nothing in
either stream implicates a harness, transport or oracle defect: every
completion refusal matches its recorded readiness state, every oracle test
passes, and both cells fail only on the typed `round_budget` exit.

M15 remains open. Per M15_ACCEPTANCE §5 this valid FAIL rejects the
attempt-incident admission candidate (source `38d458e`); the window is not
rerun. Candidate selection is a user decision bounded by the frozen route:
no Context/GC retune, no protocol-bound weakening, no round stop, no
TaskGraph, no prompt pressure. The next candidate must address how a model
converts the basis-stamped repair stage and progress facts into a converging
tail (verification current + acceptance coverage + empty
`next_action`/loops) within the existing 48-round budget, and the static
marker wording is a harness-visible fixture property that also still misses
on otherwise-correct implementations.
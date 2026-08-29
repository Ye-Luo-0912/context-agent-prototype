# Task-aware Completion Convergence paired live gate — 2026-08-29

**Decision: the true projection-off/on paired gate ran decision-grade
(8/8 cells PASS, 0 NOT_RUN, provider healthy every cell) and FAILED
promotion under the frozen strict-parity rule. The model projection stays
default-off and the gate returns to observation. Do not claim Task-aware
Completion Convergence closed, and do not claim M15 closed.**

## Identity

- command: `agent-eval --allow-dirty --conv-gate` (normal + resume, 2 paired
  repeats, off/on arms); judgment from the pure `evaluate_conv_gate`
- cells: 8 = 1 pack × 2 modes × 2 repeats × 2 arms; all `retry-pilot-cell-v3`
- pack: `retry_policy_dev` (frozen fixture, sha256
  `5055c1c5…762ca`; acceptance declaration "the retry policy behaves
  correctly" patched on by the harness in every cell)
- the single arm variable is `project_progress` (renders the bounded
  settlement fact through `PromptAssembler` in the TASK PROGRESS frame);
  `opportunity` and `recovery_surface` are off in both arms
- source: dirty tree at `git_head 2cb1c15` (`git_dirty=true`); the manifest
  records `source_tree_digest` instead of a clean identity. This is a
  Step-4 directive gate on the approved budget, not a formal M15 window
- serving: `gpt-5.6-luna` @ `https://api.pinaic.com/v1`, `protocol=responses`,
  context window 128,000
- provider health: 8/8 `healthy`; zero `NOT_RUN`; one request-level retry
  absorbed (normal off r2, `model_retries=1`); zero whole-cell reruns
  (no `r{n}-attempt{k}` dirs)

The non-arm-suffixed `retry_policy_dev-normal/` and `retry_policy_dev-resume/`
dirs are the earlier CONV-CLOSE-01 observation run (4 cells at clean
`deed96c`); their report text is preserved in git history and their causal
interpretation was superseded by review. Only the arm-suffixed `-off`/`-on`
dirs below are the cells of this paired gate.

## Results

| cell | verdict | behavior | diff | closure | continuation | rounds | wall ms | settled | episodes | round/call/fail |
| --- | --- | --- | --- | --- | --- | ---: | ---: | --- | ---: | --- |
| normal off r1 | PASS | pass | pass | completed | n/a | 34 | 296,701 | none | 0 | — |
| normal off r2 | PASS | pass | pass | completed | n/a | 25 | 522,425 | seen | 1 | 1/1/0 |
| normal on r1 | PASS | pass | pass | completed | n/a | 18 | 187,643 | seen | 1 | 1/1/0 |
| normal on r2 | PASS | pass | pass | completed | n/a | 25 | 233,715 | seen | 4 | 5/5/1 |
| resume off r1 | PASS | pass | pass | completed | restored_and_continued | 9+18=27 | 277,881 | seen | 3 | 4/4/1 |
| resume off r2 | PASS | pass | pass | completed | restored_and_continued | 5+22=27 | 243,032 | seen | 1 | 1/1/0 |
| resume on r1 | PASS | pass | pass | completed | restored_and_continued | 7+27=34 | 352,068 | seen | 1 | 1/1/0 |
| resume on r2 | PASS | pass | pass | completed | restored_and_continued | 8+7=15 | 151,884 | seen | 1 | 1/1/0 |

Exposure: 7/8 cells observed at least one `SettledCandidate` episode
(`settlement_seen=true`); the single zero-exposure cell is off normal r1.
Every cell closed through model-chosen `task.complete` → durable
`TaskCompleted`; none auto-closed.

## Episode accounting (settlement episodes)

An episode starts on entry to a task-aware candidate and ends at the first
reopening transition or terminal outcome; reopened phase-two work is charged
to new episodes, never to an earlier one. Five exposed cells (off normal r2,
on normal r1, off resume r2, on resume r1, on resume r2) each recorded one
episode of 1 round / 1 call / 0 failures. Normal on r2 recorded 4 episodes
totalling 5 rounds / 5 calls / 1 failure across them, and resume off r1
recorded 3 episodes totalling 4 rounds / 4 calls / 1 failure — the episode
split is exactly the reopened-work boundary, not a lifetime tail. Read-only `--conv-tail` over the new arm-suffixed dirs reports
per-episode composition (no_progress / redundant / advanced / failures) with
medians: normal off 0/0/0, normal on 3/0/1, resume off 2/1/1, resume on
1/0/0. With n=2 per arm per mode these are observation, not a causal claim.

## Judgment (verbatim from the runner)

```text
convergence gate: fail (off=4 on=4)
  - pair 0 (normal): settlement exposure false/true
  - pair 0 (normal): marker violations off=2 on=3
  - pair 1 (resume): marker violations off=2 on=3
  - pair 2 (normal): marker violations off=3 on=3
  - pair 3 (resume): marker violations off=3 on=2
```

`evaluate_conv_gate` returned early on parity violations, so the efficiency
reasons were not appended; evaluated on the numbers they would also fail:
episode-rounds/calls medians are 1→1 (not strictly lower) and the max
whole-cell round tail is 34→34.

## Root-cause analysis

1. **Pair-0 exposure asymmetry (off none / on seen).** Off normal r1
   (34 rounds) recorded **no trusted verification pass at all**: all four of
   its `verify.run` calls used the discovered general runner `rust.workspace`
   (`cargo test --workspace`, TaskScoped by design), which executes but
   cannot carry an exact verification identity, so no `execution_verification_pass`
   event was emitted and the task-aware join never armed — the settlement
   label stayed at `verification_due` for the whole cell even though the
   cell passed every hidden check. Every exposed cell used the
   host-registered `jobrunner.exact` recipe (source-read-only
   ExactCurrentWorld) at least once. A trusted pass is synchronous with its
   observation: the same frontier emission that records the pass then
   publishes the candidate label (observed `execution_verification_pass` →
   `settlement:"settled_candidate"` in immediate succession), so exposure is
   not delayed to a later tool call. The pair-0 asymmetry is therefore model
   **recipe-choice variance** (which of the two surfaced recipe ids the model
   picked), and per the frozen rule the zero-exposure cell is inconclusive.
2. **Marker-violation parity.** The fixture's six content-marker
   assertions are needle-text checks ("transient classification
   implemented", "retry loop bounds on max_attempts", "delay growth
   saturates at max_delay_ms", …); 2–3 are missed per cell in BOTH arms
   while the harness-owned behavioral oracle passes 7/7 in every cell
   (hidden `passed=true`, `replay_complete=true`). This is the same
   needle-shape phenomenon as the M15 diag overflow markers, unrelated to
   the projection switch; the parity rule flags any non-empty marker
   violations on either side, and 3/4 pairs also differ in count.
3. **Efficiency.** Episode medians 1→1 rounds and 1→1 calls are not
   strictly lower; the max whole-cell tail is equal (34→34). No
   measurable regression, but no measurable episode or tail reduction
   either.

Projection rendering was confirmed real and strictly arm-separated: the off
arm never rendered the TASK PROGRESS layer (`task_progress_tokens=0` every
round), while every on-arm cell rendered bounded progress blocks of 430–512
tokens in 14–33 of its rounds once a candidate existed. All 8 cells held the
truth chain under live load: behavior/diff/closure/continuation parity is
otherwise intact; the two fail families are recipe-choice exposure and the
marker dimension, not a runtime, transport or mechanism failure.

## What landed under option A and made live candidate emission possible

- trusted verification PASS clears identity-exact `failed_commands` bound to
  the current basis/directive/workspace tuple; a fresh failure re-records
  and re-blocks, so cells reached `execution_ready`;
- provider retry spans a request-level window (6 attempts, ~62 s) plus a
  whole-cell rerun wrapper (up to 3 runs, 30 s/60 s backoff) for retryable
  transport outcomes; exercised once at request level (normal off r2) and
  never at cell level;
- live acceptance data source: the harness patches the bounded acceptance
  declaration onto the task, and the runtime binds the current trusted
  verification pass as the coverage claim for every declared criterion at
  observation time. 7/8 cells then reached a task-aware candidate through
  the real boundary (task_ready = execution_ready + epoch + no open loops /
  next action + acceptance coverage).

## What this run does not claim

- not a promotion: the frozen rule ends this candidate attempt with the
  projection default-off;
- no causal projection effect is measurable from this gate (parity failed
  before efficiency);
- not a formal M15 window: `--allow-dirty` on a dirty tree; the formal
  window requires a clean source identity;
- no closure claim, no Context/GC/retrieval/prompt-packing change, no
  auto-close and no fixed-round stop were involved.

## Next step

Per the frozen CONV-CLOSE-02 rule, a failed gate leaves projection off and
returns to observation. The diagnosed causes are bounded and fixture-level:
(a) pair-0 exposure is a model recipe-choice effect — the cell that used only
the TaskScoped `rust.workspace` recipe recorded no trusted pass and stayed
inconclusive, while cells that used the host-registered `jobrunner.exact`
recipe armed synchronously; (b) the marker dimension fails parity on
needle-shape misses that the behavioral oracle tolerates. Any change must
first pass the deterministic reopening/restore suite and the frozen
efficiency criteria; no rerun before that diagnosis, and no fourth
unchanged M15 window.
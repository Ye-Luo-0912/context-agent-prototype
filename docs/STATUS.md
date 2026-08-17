# Current facts

This file is the current implementation snapshot. Historical eval and
audit narrative stays in `docs/AUDIT_TODO.md`, `docs/ROADMAP.md`, and
`crates/agent-eval/evidence/*/REPORT.md`. Do not treat
`docs/CONTEXT_RUNTIME_TODO.md` as live contract.

## Prompt and authority

- The transcript is not model context. `ContextEngine::materialize` returns
  historical working set only.
- Production engines leave `MaterializedContext.focus` and `.task` empty.
- `PromptAssembler` renders System Policy, Runtime Facts, Focus,
  TaskAnchor, TaskProgress, historical context, Turn Frame, and tool
  schemas from runtime-owned state.
- `TaskAnchor` is the only task authority. `ResumePoint` is a bounded
  operational cache applied after the durable `TurnCompleted` barrier.
- `context.collect` is not model-facing.
- `ModelStarted` carries per-layer token costs (system / facts / anchor /
  progress / focus / history / turn / tools). Sum them across rounds; do
  not treat provider tokens as one blob.

## Pack budget

```text
pack window - output - system(+facts) - focus frame - turn - tools
= ContextEngine budget
```

Focus frame = TaskAnchor + TaskProgress + Current Focus.

## Compaction

Short episodes pay for an LLM card only when the episode left a semantic
delta (durable/pinned constraint or decision, typed labels, live error, or
verified-fixed). A `FileObservation` alone does not.

## Evaluation

- Keep `semantic_recall.v1` as a long-protocol trajectory. It does not
  prove GC-forget-and-recall because the constraint lives on TaskAnchor.
- `task_switch_long_b` pass does not prove ResumePoint value.
- Do not retune reactivation thresholds from current counters.
- `recovery_auto_reactivation`, `reactivation_events`, `unique_reactivated`,
  and `reactivation_selected/consumed` are different denominators. Do not
  report 42 events → 32 unique ids as a single utilization rate.
- Next measurement: `--context-bench-ablation` (semantic_recall C-only:
  current / force-compact / no-progress, 2 repeats, shuffled arm order).
  Truncated first wave: `evidence/context-bench-ablation/` (gateway 400).
  Complete retry: `evidence/context-bench-ablation-retry/` — 6/6 hidden
  pass. Adaptive compact stayed 0/0; force-compact paid ~38k in; 
  `no-progress` kept TaskProgress at 0. Live traces are not identical, so
  this is not a token-savings proof. Do not mix either directory into the
  frozen wave-1 pack.

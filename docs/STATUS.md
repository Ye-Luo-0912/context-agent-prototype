# Current facts

This file is the current implementation snapshot. Historical eval and
audit narrative stays in `docs/AUDIT_TODO.md`, `docs/ROADMAP.md`, and
`crates/agent-eval/evidence/*/REPORT.md`. Do not treat
`docs/CONTEXT_RUNTIME_TODO.md` as live contract.

## Prompt and authority

- The transcript is not model context. `ContextEngine::materialize` returns
  historical working set only.
- Production engines leave `MaterializedContext.focus` and `.task` empty.
  Restore alignment reads `ContextDiagnostics.focus_task_id`, not
  materialized focus.
- `PromptAssembler` renders System Policy, Runtime Facts, Focus,
  TaskAnchor, TaskProgress, historical context, Turn Frame, and tool
  schemas from runtime-owned state.
- `TaskAnchor` is the only task authority. `ResumePoint` is a bounded
  operational cache applied after the durable `TurnCompleted` barrier.
  Verification facts bind to the `anchor_revision + workspace_revision`
  that produced them and never auto-promote after a later mutation.
  Verification and workspace mutation are orthogonal: a typed
  verification does not imply a non-mutating command.
- Dead ResumePoint fields (`objective` / `blockers` / `next_actions` /
  `last_cursor` / `workspace_facts_stale`) are gone. TaskProgress prompt
  projection is hard-capped (`MAX_TASK_PROGRESS_PROMPT_CHARS`).
- `context.collect` is not model-facing.
- `ModelStarted` carries per-layer token costs (system / facts / anchor /
  progress / focus / history / turn / tools). Sum them across rounds; do
  not treat provider tokens as one blob.
- `ModelUsed` carries `attempts` / `retries`. Failed attempts usually
  report no usage; when `retries > 0` the recorded provider tokens are a
  lower bound (`provider_tokens_lower_bound`).

## Pack budget

```text
pack window - output - system(+facts) - focus frame - turn - tools
= ContextEngine budget
```

Focus frame = TaskAnchor + TaskProgress + Current Focus.

## Compaction

Episodes may rotate on length or semantic distance. An LLM distill card is
paid only when the closing episode left a semantic delta (durable/pinned
constraint or decision, typed labels, live error, or verified-fixed), or
when the ablation-only `force_episode_llm_distill` override is on.
`generation >= 4` is not a distill trigger. A `FileObservation` alone
does not pay the compactor.

## Reactivation accounting

Engine reactivation counters are segment-local and zeroed on restore.
Eval `reactivation_events` / `unique_reactivated` are summed from
`ContextGc` events across the run. Selected/consumed remain engine-segment
snapshots. Do not report events and unique ids as one utilization rate.

## Context V1 operational core

ResumePoint / TaskProgress / reactivation accounting / adaptive distill
are frozen as operational fact. Do not retune `active_threshold` /
`archive_threshold` / `gc_max_generation` / reactivation scoring, and
do not change frozen Context Bench SPEC.

A follow-up C-hygiene slice is in progress on this tree:

- `WorkingSetSignal` is a structured `ResourceTouch` (path@revision).
  Shell/process stdout does not heat entities and does not become a
  successful ToolObservation's entity signature. A stamped path on
  `shell.exec` is identity only; file-body supersession stays `fs.read`.
  Successful `fs.write` / `edit.replace` / `edit.patch` stamp
  `path@revision` (patch via `metadata.files[]`) so the coding loop heats
  from trusted ResourceTouches. `ResumePoint` records those touches as
  checked `path@revision` facts; a pathless mutation still clears them.
  The prompt folds persistable open-turn tool results into `TaskProgress`
  so the current loop sees `path@revision` before the turn commits; the
  stored cache still updates after the durable barrier. Selected historical
  `fs.read` bodies whose path is already Checked are omitted from
  SELECTED WORKING CONTEXT (identity stays on the item header as
  `path@rev`); stamped-path identity logs (`shell.exec` / writes) omit
  stdout the same way when the path is Checked. Live TurnFrame tool
  results still carry the exact body.
  Materialize packing prices those covered items as descriptors
  (`ContextHints.checked_files`) so the working-set budget is not spent
  on omitted bodies; the heap still keeps the exact content.
  User-hot and tool-hot are split; tool-hot has a short TTL. Auto-reactivation uses exact typed
  match; search/scoring keep fuzzy match. Stamped-path shell/process logs
  do not hot-recall (identity is not a body); only `fs.read` file bodies
  auto-reactivate unless P3 is on, and a path already named in
  TaskProgress (`ContextAction::CheckedFiles` before GC) stays
  Warm/Stored until Fetch/Admit. Those skipped bodies still appear as
  bounded EXTERNAL CONTEXT descriptors (`context://` + `path@rev`), not
  as selected text — Warm via the eviction buffer, Stored via the entity
  index even past the recency tail. Search and inspect of those same
  ToolObservation / FileObservation items are identity cards (`path@rev`);
  file text is not a search needle. Fetch still returns the catalog body.
  This is not P3: unchecked file bodies still return.
- `fs.read` reread classes and selected-token attribution (kind / reason /
  reactivation / source) are measurement-only.
- Descriptor-only ToolObservation reactivation and `recent_file_bodies`
  cap/lease are ablation switches (default = current policy). Measure
  them with `agent-eval --context-hygiene` (engine-only, no provider).

Main engineering still returns to M12/M13 after this slice. Formal
large-scale M15 waits until a V1 candidate. Frozen `context-bench.v1`
SPEC / pack digest are untouched.

## Evaluation

- Keep `semantic_recall.v1` as a long-protocol trajectory. Do not keep
  live-running it. It does not prove GC-forget-and-recall because the
  constraint lives on TaskAnchor.
- `task_switch_long_b` pass does not prove ResumePoint value.
- Do not retune reactivation thresholds from current counters.
- `agent-eval --context-hygiene` is the engine-only P3/P4 ablation
  (current / descriptor-only / one-file-body). It does not enable those
  switches in production C and does not rewrite SPEC.
- Historical C-ablation evidence remains under
  `evidence/context-bench-ablation-retry/`. Do not mix it into the frozen
  wave-1 pack.

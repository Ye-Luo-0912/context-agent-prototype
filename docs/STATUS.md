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
- `PromptAssembler` renders System Policy, Runtime Facts, Focus
  (`TASK ORIGIN`, `PERSISTENT TASK STATE`, `CURRENT DIRECTIVE`),
  TaskProgress, historical context, Turn Frame, and tool schemas from
  runtime-owned state.
- `TaskAnchor` is the only task authority. `ExecutionState` (checkpointed
  as `resume` on `TaskRecord`) is a bounded operational cache applied
  after the durable `TurnCompleted` barrier.
  Verification facts bind to the `anchor_revision + workspace_revision`
  that produced them and never auto-promote after a later mutation.
  Verification and workspace mutation are orthogonal: a typed
  verification does not imply a non-mutating command.
  `may_mutate_workspace` is the authority/safety fence; knowledge
  freshness uses `MutationFootprint` (`None` / `Known(touches)` /
  `Unknown`). An unknown process write bumps `workspace_revision` (old
  PASS is omitted) but keeps `path@revision` facts and marks them
  `NeedsRevalidation`. Runtime revalidates up to 8 pending facts at
  BeforeModel by hashing through `ResourceVersionOracle` (no file body
  in the prompt, no extra model round).
- `TaskAnchor.original_goal` is task identity / historical origin, not
  the current instruction. Each user turn's `FocusState.current_query`
  is the highest-priority TurnIntent. Do not bump `anchor_revision` on
  every user message.
- Tool-surface policy maps `derive_needs(TurnIntent, TaskSpec, operational
  state)` → `ToolSemanticRole` on catalog `ToolSpec.roles`. NeedVerify
  prefers a declared `Verify` role, else catalog discovery
  (`capability.manage`), else an `EscapeHatch`. It does not prefer
  `InspectDiff` (`git.status` / `git.diff`) and does not encode that
  cargo verification uses `shell.exec`. A verification *obligation*
  (source change, spec change, failed verification) persists across
  user turns. NeedVerify is due *this round* only for an open failure,
  a persistent unmet obligation plus complete/coverage/soft-NL verify,
  not because `acceptance_criteria` is nonempty, not because an Unknown
  process bumped the world clock, and not because a later note turn
  follows an edit. Natural-language verify is a frozen four-needle hint
  (`run the tests` / `run tests` / `verify that` / `check that tests`);
  it is not algorithm authority and must not grow into a
  verify/check/test/confirm/validate/ensure dictionary. Reliable Verify
  triggers are typed failed verification, persistent pending
  verification, the completion gate, or an explicit tool/user control
  signal. `NeedMutate` is not inferred from a non-empty instruction. A
  new user turn replaces TurnIntent; it does not rewrite TaskSpec, bump
  `anchor_revision`, or wipe the verification ledger. C-hygiene
  (`ResourceTouch` heating, tool-hot TTL, Checked omit) stays; P3/P4
  stay ablation-only. TaskProgress `Checked` and context body-omission
  consume only `Fresh` identities (`NeedsRevalidation` / `Missing` stay
  off that projection). A structurally empty provider completion (empty
  content, no tool calls, 0/0 usage) retries at most twice while the
  turn has no persistable tool delta, then fails closed — it is not
  `TurnCompleted`. Unstamped catalog tools get a *legacy builtin name*
  role fallback only (`fs.read`, `shell.exec`, …). Unknown plugin names
  have no semantic role; the producer must declare. No builtin currently
  declares `Verify`. Do not add `verify.run(command: string)` (renamed
  `shell.exec`). Structured `verify.project` / `verify.tests` wait until
  the M12/M13 process/effect boundary is stabler, and must feed coverage
  / workspace_revision / result / artifact-log ref into ExecutionState.
  Current-directive exact-mentions of known ExecutionState paths may be
  projected as `CURRENT FOREGROUND EVIDENCE` (max 2 resources, ~2048
  tokens, latest exact revision). That is passive transient rehydration:
  Warm stays Warm, Stored is not Admitted. Do not keep 8 file bodies
  Resident as a substitute; after this projection is measured, P4 can
  test 8 → 1/0 without extra rereads.
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
are frozen as operational fact. This core is a **freeze candidate**. Do
not retune `active_threshold` / `archive_threshold` / `gc_max_generation`
/ reactivation scoring, and do not change frozen Context Bench SPEC.

Latest C live: `reactivation_events=1`, `unique=1`,
`reactivation_selected=0`, `reactivation_consumed=0`,
`reactivated_tokens=48`. Auto-reactivation is no longer the extra-round
driver. Retuning those knobs would chase a problem that is gone.

Ownership:

```text
Task authority            → Runtime / TaskAnchor
Operational state         → ExecutionState
Historical selected       → ContextEngine
Exact old evidence        → Catalog / Search / Fetch
Body residency            → GC
Prompt                    → Runtime assembler
```

Execution Coherence V1 is four phases, not more heuristics. There is no
planner. The LLM still chooses actions. Runtime only maintains provable
world state. Contract: `crates/agent-runtime/src/execution/mod.rs`.

```text
World Facts (path@rev / errors)
  → Freshness Engine (Fresh / NeedsRevalidation / Missing)
  → Obligation Ledger (verify / failure / unresolved evidence)
  → Round Projection (due_now / foreground refs / missing evidence)
       → Prompt and Tool roles → LLM
```

Invariants:

1. Unknown ≠ False, and NeedsRevalidation ≠ Fresh. Do not delete facts
   to hide uncertainty.
2. Obligation exists ≠ Due now. Do not wipe a real obligation just to
   avoid surfacing Verify.
3. Resource identity known ≠ body available in prompt.

Do not add Typed EpisodeOutcome, a smarter reactivation scorer, vectors,
embeddings, RAG, a learned router, or a new GC generation algorithm. No
evidence requires them.

The C-hygiene follow-up and tool semantic roles landed on this tree:

- Operational state lives in `agent-runtime/src/execution/`:
  `ExecutionState` (checkpointed as `TaskRecord.resume`), freshness
  (`MutationFootprint` / revalidate), `derive_execution_needs`, and an
  ObservationMemo stub that is not wired into dispatch. `TaskAnchor`
  remains the only task authority; there is no third task table.
  Prompt framing is `TASK ORIGIN` / `PERSISTENT TASK STATE` /
  `CURRENT DIRECTIVE`. `original_goal` is task origin, not a perpetual
  instruction. User turns replace TurnIntent and do not patch
  the anchor.
- `WorkingSetSignal` is a structured `ResourceTouch` (path@revision).
  Shell/process stdout does not heat entities and does not become a
  successful ToolObservation's entity signature. A stamped path on
  `shell.exec` is identity only; file-body supersession stays `fs.read`.
  Successful `fs.write` / `edit.replace` / `edit.patch` stamp
  `path@revision` (patch via `metadata.files[]`) so the coding loop heats
  from trusted ResourceTouches. `ExecutionState` records those touches as
  `ResourceFact` rows (`path`, SHA-256 revision, `Freshness`). Authority
  still treats `shell.exec` / `process.run` as may-mutate; an unknown
  (pathless) footprint must not `checked_files.clear()`. It marks known
  identities `NeedsRevalidation` and the runtime re-hashes them.
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
- `fs.read` engine residency classes stay measurement-only, now split by
  last-prompt exposure: body-visible (`previously-selected`), packed
  `path@rev` (`selected-descriptor`), EXTERNAL CONTEXT `path@rev`
  (`external-descriptor`), plus resident / warm / stored / first-read.
  Runtime stamps an E2E motive on each `fs.read` ToolFinished:

  | Motive | Meaning |
  | `first` | Normal first exploration |
  | `body-visible-current` | Body was in the last prompt; model trajectory |
  | `descriptor-only` | Last prompt had identity only; model needed the body |
  | `checked-fresh` | Identity known and the body had no clear need |
  | `needs-revalidation` | Runtime should hash; the model should not `fs.read` |
  | `warm` | GC moved the body to the eviction buffer |
  | `stored` | Deeper GC rehydration from the store |
  | `changed` | Digest actually moved; a reread is justified |

  Old traces with `selected-current` still parse as body-visible.
  ObservationMemo stays unwired (`lookup()` is always a miss). It saves
  tool I/O after the model already chose `fs.read`; Foreground Evidence
  is the cheaper path. First wired version is `fs.read` only, keyed by
  path + line range + content revision. Do not memo `search.grep` /
  `git.diff` / `git.status` until a workspace snapshot identity exists.
  Never memoize writes, patches, or shell side-effects.
- Descriptor-only ToolObservation reactivation and `recent_file_bodies`
  cap/lease are ablation switches (default = current policy). Do not
  shrink the default of 8 until Foreground Evidence Projection is
  measured against extra rereads. Measure P3/P4 with
  `agent-eval --context-hygiene` (engine-only, no provider).

After this freeze write-up, main engineering is M12/M13. Generic
`shell.exec` / `process.run` / `process.session` now fail closed without
Core-issued effect identity and cannot disguise child mutations as a
prepared effect; they still do not roll back work the child already
performed. The approved `ProcessRun` bound is the lexical command they
will spawn (`command` vs `argv`); spawn fails closed unless it covers
the actual command. `process.session` poll/stop are not a new spawn:
leftover `argv` cannot consume a command-prefix grant. Session recovery
is keyed by the start identity (stop settles start as `CompletedValue`;
poll/stop identities stay `NotApplied`). Mismatched process-tool identity
and a parent-escaping session cwd fail closed before spawn. The dispatcher
refuses session start/poll/stop without Core identity and refuses attaching
an effect identity to ReadOnly `git.status`. Remaining M12/M13: Linux UDP/raw/pathname-Unix,
Linux absolute reads, Windows OS-level network confinement, and
I/O bandwidth quotas (seccomp/AppContainer stay out of v0).
Linux TCP bind/connect is landlock-denied on ABI v4+ (`MOD-07`) when write roots
are set. ABI v5 denies device ioctl (`MOD-12`). ABI v6 also scopes outbound signals (`MOD-11`). Windows Low-IL write confinement is landed (`MOD-08`). Unix
`RLIMIT_AS` is landed (`MOD-09`). Unix `RLIMIT_FSIZE` is landed (`MOD-10`). Unix
`RLIMIT_NOFILE` and inherited-fd close are landed (`MOD-13`). The Windows
integrity wrap Job-Object caps the real child's commit at 512 MiB (`MOD-14`).
Unix `RLIMIT_CORE` is forced to zero when sandbox `pre_exec` runs (`MOD-15`).
Linux `RLIMIT_NICE`/`RLIMIT_RTPRIO` are clamped to zero and `no_new_privs`
is set in that same hook (`MOD-16`). Windows Job-Objects pin
`PRIORITY_CLASS=NORMAL` with breakaway default-deny (`MOD-17`). Protocol
work proceeds beside that residual: `PLAT-05` landed (`ProcessSupervisor` +
`DuplexTransport`; stdio first backend; `ProcessHost` and MCP stdio both
kill then await reap). `PLAT-06` slice 1 landed (`ConnectionHealth` /
`ConnectionEpoch` / bounded `RestartCircuit`; first connect is not a
restart). `PLAT-06` slice 2 landed (peer cancel-ACK + coalescible
progress; cancel before write does not poison; kill-then-reap remains
settlement). Remaining PLAT-06: multiplexing (stay single-inflight). Named Pipe/UDS
remain `PLAT-08`. After MOD-17 there is no further allowed v0 sandbox
slice; do not invent `MOD-18` from UDP/raw/pathname-Unix, Linux absolute
reads, Windows OS-level network, I/O quotas, or from multiplexing /
Named Pipe/UDS. M12, M13, and PLAT-06 stay open.
Closing the remaining OS isolation (and later M13 sandbox) is what
closes the path to dynamic plugins and Self-Iteration. Formal
large-scale M15 waits until a V1 candidate. Frozen `context-bench.v1`
SPEC / pack digest are untouched.

Production `ToolLifecycleConfig::default()` always-loads `fs.list`,
`fs.read`, `search.grep`, `artifact.read`, `task.complete`,
`capability.manage`. Git / shell / write / edit / `context.manage` are
catalog-only. Live coding compare (`--compare-live`,
`--compare-live-reasonable`, `--pilot-run`, `--fixture-live`) reuses
that production default. Scripted `--compare-arm`, fixtures, and
context-bench/mech ops still pin `fs.write` / `edit.replace` and
`context.manage`. Runtime PreferSurfaces `context.manage` when
BeforeModel diagnostics show Warm/Cold/Stored catalog entries or the
active TaskAnchor has `evidence_refs` (NeedEvidence / EXTERNAL CONTEXT
safety net for Late Semantic Recall). The model can also
`capability.manage load context.manage`.

Tool-quality preflight (`TOOL-ENV-01` → `TOOL-ERROR-01`) is closed in
code (2026-08-17): Runtime Facts, disclosed `shell.exec` dialect,
revision-aware `edit.replace`, ordinary-view hide of `.focus-agent` /
`.git`, and trusted `ToolFailureClass` projection. The TOOL_ECOSYSTEM
"Current code baseline" table listed those items open until 2026-08-21
(docs-only). Another Context Bench live wave is a later-milestone
frozen-cell rerun, not v0 engineering. Frozen SPEC / pack digest are
untouched.

## Execution Coherence closeout (items 21–29)

Source: operator freeze list 2026-08-19 (not a ROADMAP / CORE-ID /
compatibility-order numbering). Verified against code 2026-08-21. This
is the durable closeout list.

- [x] **21** Auto-reactivation left the extra-round problem (latest C:
  `reactivation_events=1`, selected/consumed 0, 48 tokens). Do not
  retune `active_threshold` / `archive_threshold` / `gc_max_generation`
  or the reactivation scorer. Pinned by
  `gc_thresholds_are_freeze_pinned`.
- [x] **22** Context operational core is a freeze candidate. Ownership:
  TaskAnchor / ExecutionState / ContextEngine / Catalog-Search-Fetch /
  GC / assembler. Do not add Typed EpisodeOutcome, a smarter scorer,
  vectors, embeddings, RAG, a learned router, or a new GC generation
  algorithm.
- [x] **23** Live coding compare (`--compare-live`,
  `--compare-live-reasonable`, `--pilot-run`, `--fixture-live`) reuses
  production `ToolLifecycleConfig::default()`. Scripted `--compare-arm`
  and context-bench/mech ops still pin write/edit and `context.manage`.
  Frozen context-bench SPEC is untouched.
- [x] **24** `context.manage` is catalog-only on the production
  `ToolLifecycleConfig::default()` surface. Runtime PreferSurfaces it
  when BeforeModel diagnostics show Warm/Cold/Stored catalog entries or
  TaskAnchor `evidence_refs` is nonempty (NeedEvidence / EXTERNAL
  CONTEXT). `capability.manage` stays always-loaded so the model can
  load it. Scripted pin still always-loads it. Live coding compare
  reuses production default. Pinned by
  `context_manage_is_catalog_only_on_the_production_surface` and
  `live_coding_compare_uses_production_tool_surface`.
- [x] **25** ObservationMemo stays unwired (`lookup()` is always a
  miss). First wired version, when allowed, is `fs.read` keyed by path +
  line range + content revision. Never memoize writes, patches, or
  shell.
- [x] **26** Execution Coherence V1 is four phases (World Facts →
  Freshness → Obligation Ledger → Round Projection → Prompt / Tool
  roles → LLM). No planner.
- [x] **27** Three invariants: Unknown ≠ False and NeedsRevalidation ≠
  Fresh; obligation exists ≠ due now; resource identity known ≠ body in
  prompt.
- [x] **28** `recall_after_fix` diagnostic mission is complete. Scripted
  `--compare-arm` stays. Live `--compare-live` / `--fixture-live` /
  `--compare-live-all` refuse it. `--compare-live-reasonable` is only
  `add_test`. Next mechanism live is `--context-mech` /
  `--context-mech-run`.
- [x] **29** After this closeout, main engineering is M12/M13 (and
  PLAT-06 beside the residual). This item does **not** close M12, M13,
  or PLAT-06. Remaining OS isolation and multiplexing are out of v0.

## Evaluation

- Keep `semantic_recall.v1` as a long-protocol trajectory. Do not keep
  live-running it. It does not prove GC-forget-and-recall because the
  constraint lives on TaskAnchor.
- `recall_after_fix` diagnostic mission is complete. Keep scripted
  `--compare-arm` tests. Live `--compare-live` / `--fixture-live` /
  `--compare-live-all` refuse it. `--compare-live-reasonable` runs only
  `add_test`.
- Next mechanism live is `agent-eval --context-mech` /
  `--context-mech-run`: `late_semantic_constraint` (non-Anchor semantic
  recovery after GC), `resume_operational_state` (verify → mutate →
  switch → resume freshness), `no_semantic_episode`.
- `task_switch_long_b` pass does not prove ResumePoint value.
- Do not retune reactivation thresholds from current counters.
- `agent-eval --context-hygiene` is the engine-only P3/P4 ablation
  (current / descriptor-only / one-file-body). It does not enable those
  switches in production C and does not rewrite SPEC.
- Historical C-ablation evidence remains under
  `evidence/context-bench-ablation-retry/`. Do not mix it into the frozen
  wave-1 pack.
- `recall_after_fix` NeedVerify live compare (n=1, 2026-08-19) is under
  `evidence/roles-verify-recall/`. C extra-round leftover vs the
  C-hygiene diagnosis dropped (23r/35t → 14r/13t unpinned). Hidden
  checks stay mixed. Not M15. Live coding compare now reuses production
  `ToolLifecycleConfig::default()`; historical unpinned cells still pinned
  write/edit and are not production Tool Surface.
- Production-surface `--compare-live-reasonable` (`add_test`, n=1,
  2026-08-21) is under
  `evidence/compare-live-reasonable-2026-08-21/`. Hidden 3/3, C−A=0.
  Rounds jumped to 10–11 because write/edit are catalog-only and must
  `capability.manage load`; per-round C input is ~6.2k and does not beat
  A. Not M15.

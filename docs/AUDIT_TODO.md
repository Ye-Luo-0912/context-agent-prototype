# Audit follow-up

Confirmed defect queue. Only headings explicitly marked **open** or
**reopened** are actionable. This file still carries some clearly labelled
fixed/closed chronology that predates the compact archive; treat it as
non-actionable context and move it to the archive/git history when that section
is next touched. Do not reopen closed work under a new id without new evidence.

- Invariants: `AGENTS.md`
- Now/freeze/P0: [`STATUS.md`](STATUS.md)
- Execution: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
- Sandbox/M12: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md)
- Gates: [`ROADMAP.md`](ROADMAP.md)

M12/M13 must close before Self-Iteration. Do not add a database, vector
search, or learned ranking. Do not claim a milestone complete because
happy-path tests pass.

## Live execution queue — 2026-09-04

This index is the operational entry point. The detailed records below remain
the source for root cause and exit tests; old sections named “Open P0/P1” are
historical headings and do not override this table. There is no confirmed open
P0 on the selected default product path, but the following P1/conditional work
is not closed.

| When | Items | Disposition |
| --- | --- | --- |
| Before the Reliable Local Agent alpha | `COMPOSE-LIFECYCLE-01`, `EFFECT-ACK-01` | Close the remaining startup type error and durable ACK-debt lifecycle. These are correctness/safety exits, not product polish. `CONTEXT-IO-01` closed on 2026-09-04. |
| Before the next formal M15 candidate | ~~TOOL-MANIFEST-01~~ (closed on `9e00299`: the evaluated surface digest persists on every cell manifest; CI `33789350980` green) | ~~Record one clean local/Windows/Linux source and persist the evaluated tool-surface identity~~ — done: clean source `0668002`/`fe6a743`, CI `33785349225`/`33789350980`. A valid historical FAIL is never repaired in place. |
| Only if service Context ships as V1 | `SIDECAR-ERROR-01` | Otherwise keep `--context=service` explicitly experimental and out of the supported product profile. |
| Before long-horizon breadth, if measured | `FAILURE-SPILL-01`, `TOOL-CONTRACT-01` live acceptance | Activate spill work only if the bounded hot set overflows; use paired task evidence for tool-contract convergence. |
| Before extension promotion or Self-Iteration | `CAP-OBS-01` residual | Retire legacy producer metadata only through the existing typed-facts migration; dynamic producers never mint trusted execution facts. |

The successor is a recorded clean-source exit now: `0668002` and the digest-wiring commit `fe6a743` pass the complete local gate and Windows/Linux CI — runs `33785349225` and `33789350980` — after the reaper and fd-reuse fixes.

## Continuation review verification — 2026-09-03 (`c823a1c`)

The supplied continuation-review synopsis was independently checked against
the exact clean `main` above, with `ea8deefc873abee13106de92bbbb3ddbaeb2d423`
as its stated comparison baseline. A recovered copy of the original conversation
shows that its final file read already failed with `FileNotFoundError`, the final
message carried `attachments = null`, and the last fetched `main` was
`e357bed`; the three `sandbox:/mnt/data/context-agent-prototype-audit/...`
links were therefore never registered artifacts and are not evidence for any
status below. The source gate was independently rerun: format, all-target /
all-feature check, strict Clippy, all-target build and the complete all-target
workspace suite pass (the Windows test run used the bundled Python 3.12 rather
than the non-functional Windows Store alias).

The review correctly identifies a missing cross-run query/projection plane, no
authoritative structured TaskGraph, and an incomplete product operations
surface. Those are post-M15 direction, not evidence that the existing Runtime,
Context, recovery or effect substrates are absent. Its implementation list is
partly stale: committed-prefix replay, required-context misses, strict provider
parsing, transactional startup/admit slices, typed ACK debt, durable execution
facts, sidecar framing errors, tool-inventory parity and split prompt accounting
already have landed code. This tranche therefore creates no duplicate defect
ids. It narrows the remaining seams under `COMPOSE-LIFECYCLE-01`,
`CONTEXT-IO-01`, `EFFECT-ACK-01`, `SIDECAR-ERROR-01` and
`TOOL-MANIFEST-01`, and records `DURABLE-FACTS-01` plus `EVAL-ACCOUNT-01`
closed below.

The synopsis's settlement off/on design is not formal M15. The frozen formal
window remains TaskProgress-on / settlement-off; only a future candidate that
enables settlement projection needs the separate same-checkpoint causal fork.
The directional Chronicle/TaskGraph order is recorded as a guarded post-M15
proposal in [`ROADMAP.md`](ROADMAP.md#post-m15-candidate-order-proposal).

## Current merged audit — 2026-08-30 (`ea8deef`)

This section was the authoritative merged-audit tranche and active pre-M15
route as of 2026-08-30. It combines the repository audit with the
instruction/runtime audit against the exact source above. The later 2026-08-31
repository-wide tranche supersedes this section's route/status where they
conflict; older non-conflicting open entries remain in the backlog. Raw reports
and checker verdicts remain immutable.

Evidence interpretation:

- the audited `ea8deef` source was not green: `cargo fmt --all -- --check`
  failed and both CI jobs stopped at formatting. The merged audit landed as
  recorded source `a3bd23f`, and the `BASELINE-01` chain below repaired every
  formatting/CI/live-settlement regression exposed since and banked both CI
  platforms on the recorded source (2026-08-30);
- the 2026-08-29 convergence report remains a **mechanical FAIL**, but it is
  **causally INVALID/CONFOUNDED** for settlement effectiveness. Its arm switch
  removed the whole `TaskProgress` projection and also changed checked-file GC
  projection; it did not isolate settlement;
- formal M15 remains `3 fixtures × 2 modes (normal/resume) × 2 repeats = 12`
  cells. Settlement off/on is a separate candidate experiment, never an M15
  arm and never a retroactive M15 pass dimension;
- the existing M12/M13 closure-audit artifacts stay banked and immutable. The
  2026-08-31 effect/coordinator/Landlock findings later reopened the conclusions
  needed for a new closure claim, not those historical artifacts. No new
  M12/M13 closure claim or Self-Iteration transition is authorized until those
  findings and `GOV-STATUS-01` are resolved;
- no new live evidence or formal M15 window is decision-grade until the
  baseline and selected-path P0 items below are closed. A settlement-changing
  candidate includes the live causal-fork exit; a settlement-off base does not.

**Recorded merged-audit source (2026-08-30; `a3bd23f` plus the
`BASELINE-01` commit chain; not an evidence source by itself).** The merged
audit contains candidate implementations for the unified completion join,
continuation epoch, criterion receipts and host coverage-declaration identity,
domain-scoped failure matching with fail-closed overflow, required-context
misses, prospective terminal checkpoints, explicit commit barriers,
committed-prefix replay, a one-shot startup gate, strict provider parsing,
stable episode pairs and same-state settlement request auditing. The recorded
tree passed the four local commands on 2026-08-30 after repairing the
integration regressions. This closes no heading by itself: `BASELINE-01` is
now closed on the recorded source (both CI platforms banked 2026-08-30; see
the item below). The live causal runner still needs a common
pre-exposure checkpoint/workspace fork only if the selected candidate enables
settlement projection.

## Post-window execution and JSON audit — 2026-08-31 (`ac2eb2a`)

The JSON-hardened v4 window at
`evidence/m15-window/_windows/1788115951355/` is a valid FAIL (10/12,
0 NOT_RUN). It proves behavior/diff 12/12 and healthy provider state, while the
new malformed-tool retry recovered two format incidents. It does **not** prove
the model stopped emitting malformed JSON. Both failing policy-normal cells
completed the functional work but exhausted 48 rounds after `task.complete`
refusals. The immutable diagnosis localizes three independent defects:

- Core rejected early `shell.exec` calls before dispatch because the tool was
  absent from the captured surface, yet Runtime persisted those attempt
  incidents as permanent failed-command completion debt;
- broad verification and later shell checks repeatedly invalidated the exact
  acceptance receipt, while the refusal named counts rather than an ordered,
  executable repair protocol;
- the standing JSON sentence and the buffered retry landed together, so the
  zero-final-malformed outcome cannot be attributed to prompt wording. Product
  live streaming also treated internal tool-call deltas as already published,
  unlike the buffered evaluator.

No further formal window is authorized from the unchanged candidate. The
repair candidate was recorded in `7dc9f46` with its actor regression in
`c8b9dbb`; it remains deterministic code only and has no live-evidence status.

### EXEC-INCIDENT-01 — separate attempt incidents from task obligations (**closed on source: candidate `7dc9f46`/`c8b9dbb`, dual-platform CI green through `6fdb4f0`**)

An off-surface call is `RejectedBeforeDispatch`: no approval, dispatch or
effect occurred. It remains a typed `ToolFinished` failure and counts in model
wire-quality metrics, but must be `TransientNoPersist` and must not enter
Context, `failed_commands`, execution obligations or completion readiness. The
candidate adds trusted `SurfaceUnavailable` classification and that disposition;
free-text diagnosis cannot mint the class, so only Core's captured-surface
rejection receives the transient privilege.
Real process/verification failure, ambiguous effect settlement and rooted typed
precondition debt remain blocking.

Exit requires an actor-level regression showing an unloaded canonical command
is refused and visible, then a correct implementation plus current exact PASS
can close without replaying the rejected command. Canonical dotted and provider
wire spellings must have identical debt semantics.

### COMPLETION-REPAIR-01 — derive an executable repair protocol (**closed on source: durable stage `615b5ed`, proof-refresh transaction `b148b4d`/`d92b250`, deterministic matrix regressions `b554058`/`8dfa452`/`93e434a`/`7026d8a`)

`CompletionReadiness` remains the sole authority. A refusal now derives
one `completion-repair.v1` stage stamped with task/verification/workspace
revisions. It selects only the current highest-priority blocker class; it does
not predict later steps. Proof repair names only a `recipe_id` revalidated
through current trusted host attribution; if no exact route exists it returns
`operator_required` rather than turning a coverage-domain label into an
argument. Typed metadata identifies the refusal snapshot basis. The immediate
ToolResult is terse; while TaskProgress
projection is active, Runtime replaces its bounded model-visible
completion-repair record from current readiness every decision, so a partial
repair cannot leave the previous recipe or anchor revision authoritative.
`task.manage`/`verify.run` use `PreferSurface`; a helper load or schema-budget
miss therefore cannot abort the model round. `task.complete` remains a standing
control surface and may explicitly re-propose to refresh the stage. Refusal
ToolResults are `TransientNoPersist`; the current turn re-derives repair from
Runtime state rather than Context prose. Durable cross-turn/deferred-refusal
repair state remains part of the next slice below.

The remaining slice makes the state explicit and durable across the deferred
safe-point refusal path, carries criterion/context details directly from
`CompletionReadiness`, and adds an optional Runtime-owned proof-refresh
transaction: only when proof is the sole blocker, an explicit `task.complete`
intent may run the host-declared exact verifier under one pre/post world fence,
recheck the same basis and commit. It never bypasses open loops, effect debt,
recovery or approval.

The deterministic matrix must also prove criterion A PASS changes the next
projected stage to criterion B/recipe B, and bind resolver availability to the
same captured surface/load result rather than catalog-name presence alone.

The current candidate has passed format check, strict all-target/all-feature
Clippy, all-target build and the complete local all-target workspace suite
after rebuilding the freshness-guarded context service binary. That validates
the first slice locally; it does not replace the remaining deterministic
matrix, a recorded clean source or dual-platform CI.

### JSON-RECOVERY-01 — make format recovery product-equivalent (**closed on source: `1768914`/`f528a92`/`328ec5d`; shared formal-path/product observer wired in `7e02488`**)

The candidate removes JSON syntax advice from the standing system prompt.
`ModelEventSink` now declares whether a delivered chunk creates an irreversible
replay boundary; Runtime's live sink treats tool-call deltas as internal and
text as irreversible. A malformed tool-call body therefore gets at most one
immediate regeneration in both product and buffered eval, independently from
the transport retry/backoff budget. The two credits have one aggregate ceiling
(`transport_attempt_limit + format_credit`), and the replay barrier is set
before arbitrary sink code so publish-then-error cannot duplicate output.
Persistent malformed output still fails closed; a published text prefix is
never replayed. Both provider paths now
bound an unterminated SSE line inside `LinesCodec` by the configured total
stream cap, so raw bytes cannot grow past the boundary while waiting for `\n`.

Remaining protocol exits before another live window:

- `1768914` repairs the `dfb9ade` Responses/Chat terminal-state residual:
  identity is global to the call, terminal snapshots are compared instead of
  silently overwritten, and every terminal transition is idempotent-or-
  rejected, with direct regressions;
- `f528a92` repairs the `c55429c` observer residual: terminal records carry
  call identity and typed terminal stages (including cancelled and gave-up),
  the JSONL cap uses exact remaining-byte accounting under a per-process
  gate, and the published-stream failure path no longer subtracts a retry
  that was never reserved;
- `328ec5d` makes the retained SSE `event:` name versus the JSON payload's
  declared type fail closed instead of silently preferring one identity;
- `7e02488` completes the observer wiring: the JSONL observer moved into
  `provider-openai`, so the formal eval paths (which build their transport
  through the eval driver) and the product composition root
  (`agent-compose::model_from_env`) share one bounded implementation; the
  eval driver keeps resolving `OPENAI_RETRY_METRICS_FILE` through eval.env.

The formal long-live/M15 provider path therefore persists typed
incident/stage records whenever `OPENAI_RETRY_METRICS_FILE` selects an
artifact; without it the stderr retry line stays the human channel. The
channel remains a best-effort diagnostic artifact, not durable formal
evidence: a cell verdict never depends on it.

### EVAL-PREFLIGHT-01 — one hermetic developer/evaluation gate runner (**closed in two steps: base doctor on `1c94c31`; complete parse-first CLI and semantic Python/helper probes on `b44ea44`, with the current Linux fixture-reaper CI correction still awaiting a clean-source record**)

Landed slice (2026-09-03, `131c82f`): `agent-eval` now parses and validates
every global option (`--repeats`, `--evidence-dir`, `--include-swebench`,
`--file-only`, `--allow-dirty`, `--pilot-id`) in one pre-pass before any
subcommand runs, so option order is semantically irrelevant and unknown,
duplicate or missing values fail before files are created or processes
started; the ignored trailing `--evidence-dir` class is closed with direct
regressions.

Landed slice (2026-09-03), with successor hardening later the same day: the
dev-only `agent-eval --doctor` gate runner
covers the remaining scope in one bounded command. It probes the exact
git/cargo/rustc executables and resolves Python through the same bounded
semantic probe as hidden commands and verification discovery. Explicit
`AGENT_EVAL_PYTHON` / `AGENT_PYTHON` values (including values loaded from
`eval.env`) take precedence, followed by `py -3`, `python3`, and `python`;
the Windows Store stub becomes a typed setup failure instead of exit 9009,
and virtual-environment symlinks remain intact. The service-owned integration
target uses Cargo's exact `CARGO_BIN_EXE` helper, with no freshness timestamp
or manual build/touch rule. Doctor sends one tiny request through the exact pinned
model/protocol data plane instead of treating `/models` as serving proof
(skipped, never failed, when no key is configured), then runs the same
format/check/Clippy/build/test list CI runs, and writes one bounded
markdown + JSON readiness report into a unique non-overwriting
`target/doctor/<timestamp>-doctor/` directory that references the source
tree digest, the suite pack state and the serving identity without secrets.
It is a derived check only: it never writes `STATUS.md`, is never a second
evidence authority, and never chains into the formal preflight or the
predeclared window.

Dogfooding caught two real defects before landing, both fixed: the helper
probe initially required a binary the helper crate does not ship (now it
mirrors the CI rule exactly), and the first runner draft read gate pipes
only after child exit, so the workspace test list filled the OS pipe buffer,
blocked the child forever and burned the whole timeout — the drain now runs
on worker threads while exit is polled, the captured tail survives a
timeout, and the final verification run completes the full local gate chain
in about ten minutes with every step green.

Exit tests cover complete option/action parsing before side effects, the
Windows Store Python alias ahead of a real interpreter, no usable interpreter,
bounded probe output/time and Cargo-owned helper identity,
`/models` healthy with the actual Responses data plane unavailable, exact
evidence-directory selection, collision refusal, bounded logs, manifest-digest
mismatch and proof that the runner does not start a formal window. This work may
land independently, but it cannot satisfy M15 candidate selection or convert
any retained FAIL into `NOT_RUN`.

### TOOL-SCHEMA-VALIDATE-01 — validate the captured schema before approval (**closed on `33d0395`**)

JSON syntax is insufficient. Compile a bounded `SchemaProfile` once per tool
catalog revision, then validate arguments against the immutable round surface
before approval or dispatch. Start with the repository's actual subset:
object/properties/required, primitives, enum, arrays/items/bounds and
`additionalProperties`. Unsupported keywords fail capability admission rather
than being ignored. A mismatch returns a typed no-dispatch result with a JSON
pointer and expected shape; it never opens completion debt. Actual effect
authority remains `HostToolPolicy`, not the schema.

Exit includes catalog meta-validation and a shared corpus proving the central
validator and each builtin parser agree. Missing fields, wrong union branch,
extra fields, depth/node/byte overflow and duplicate keys must reach neither
approval nor the effect journal. Provider-native strict schemas are enabled
only through an explicit, source/serving-pinned capability; fallback validation
remains authoritative for compatible relays.

### TASK-PROGRESS-PACK-01 — pack facts and advisories as atomic records (**closed on `531a77e` + `df2f72c`**)

Replace character slicing of the mixed TaskProgress prose with bounded records
classified as hard Runtime blocker, model-resolvable fact, neutral observation
or edge-triggered advisory. Pack whole records by priority; never cut a record
mid-line. The same failure/basis appears once, stall/frontier advice is emitted
only on state transition, and required-context misses name a bounded recovery
affordance. This changes projection/observability only; Context selection, GC,
retrieval and the transcript remain frozen.

## Repository-wide architecture audit — 2026-08-31 (reviewed through `c55429c`; repaired 2026-08-31 → 2026-09-02)

This tranche records a whole-repository static review of the production Cargo
graph, Core/Runtime/Context transactions, process and sandbox boundaries,
storage/replay, and the formal evaluator. The review began from `c8b9dbb`, then
incorporated the recorded `dfb9ade` provider-protocol delta and `c55429c` typed
retry-observer delta. Later concurrent uncommitted task/completion edits were
deliberately left untouched and carry no audit/evidence status. Older audit
entries remain authoritative where they do not conflict with the narrower
findings below.

**Repair status (2026-09-03):** every P0 and P1 implementation finding below
now has a landed repair in `615b5ed..6fdb4f0`, and dual-platform CI is green
on `6fdb4f0` (run `33624084700`). The item headings carry the repair commit.
The 2026-09-03 tranche also recorded the M10 fault-gate re-audit (see
`RUNTIME-CONTEXT-COMMIT-01`), closed `GOV-STATUS-01` on `bba1c76`, and
committed the actor protocol-body regression on `e357bed`, and wired the
formal-path/product retry observer on `7e02488` (closing the
`JSON-RECOVERY-01` residual). Every confirmed P0/P1 in this file is now
closed on source; the remaining pre-window exits are operational: one
recorded clean source with dual-platform CI, selected-path P1 spot-checks if
the candidate changes, and the M15 gate sequence itself. Treat each item
below as closed-on-source unless its heading says otherwise.

The clean-source closure exit is now banked (2026-09-03): every open heading
below closed on `97a7719` with the full local gate green (fmt, all-target
build, strict all-feature Clippy, complete all-target workspace suite,
`TEST_EXIT=0`) and seen green on dual-platform CI run `33709924715`. The
remaining operational gates are the M15 gate sequence itself.

This is the current route/status authority inside this file. It supersedes the
2026-08-30 and post-window ordering where a newly confirmed P0/P1 changes the
gate; it does not rewrite historical experiment verdicts.

The existing selected-path rule still applies. `M15-RAW-EVIDENCE-01` is always
on the formal-report path. Every other new P0 must close if the candidate uses
that path, or have exact source/surface/OS evidence proving it is not exercised;
“conditional” is not a silent waiver.

The review also confirmed the important negative facts: Core still owns no
authoritative transcript/task/turn/prompt-frame state; Runtime remains the sole
turn actor; `PromptAssembler` consumes `MaterializedContext`; no production
Cargo path currently matches the seven explicitly forbidden dependency pairs;
and token pressure is still final packing rather than a forgetting trigger.
Green happy-path tests do not cover the crash, corruption, saturation and
concurrent-lifecycle windows below.

Validation snapshot:

- the full all-target workspace suite, format check, strict all-feature Clippy
  and conformance suite were green on the pre-delta candidate represented by
  `c8b9dbb`;
- on `c55429c`, format and strict Provider/Eval Clippy passed, all 81 Provider
  tests passed, and the dependency conformance test still passed (which also
  demonstrates its guard blind spot);
- a complete all-target workspace run was not banked for `c55429c`; concurrent
  uncommitted Runtime work changed under that command and was excluded from
  this review cut. No local result substitutes for recorded dual-platform CI;
- the repair tranche ends at `6fdb4f0` with dual-platform CI green on that
  exact source (run `33624084700`, 2026-09-02). The actor-level
  protocol-body regression (`e357bed`), the doc tranche (`bba1c76`,
  `3888553`, `345fbd0`), and the observer wiring (`7e02488`) are the tree
  deltas beyond that recorded source.

### New P0 — authority, recovery and acceptance truth

#### EFFECT-ACK-CLASS-01 — preserve the typed settlement through broker ACK (**closed on `6112ffd`**)

`EffectBrokerAck` stores only `applied: bool`. Core maps every receipt except
`NotApplied` to `true` (`agent-core/src/port.rs`), while both journaled broker
paths persist that boolean and recovery turns `true` into
`Applied { durability: Durable }` (`agent-core/src/broker.rs`). A crash after a
broker ACK but before the Core terminal record can therefore upgrade both
`Unknown` and `Applied { durability: DurabilityFailed }` into durable success.
This is authority laundering, not merely missing diagnostics.

Replace the boolean with a versioned typed settlement carrying at least
`NotApplied | Applied(durability) | Unknown`; preserve it in the coordinator
wire/journal and reject unknown future variants. Exit tests reopen every receipt
class after the ACK/Core-terminal crash window and prove no recovery path can
strengthen it. `EFFECT-ACK-01` still owns failure to persist/send the ACK itself;
it must consume this typed result rather than invent another truth source.

#### RUNTIME-CONTEXT-COMMIT-01 — make turn start and checkpoint maintenance transactional (**closed: repair `9ba85d3`/`f42a898`/`f622cf3`; M10 fault-gate re-audit recorded 2026-09-03 on `e357bed`**)

For an existing task, `begin_applied_turn` ingests the user message, appends
`UserMessageAccepted`, then runs `UserInput` maintenance with direct `?`
returns. An applied-but-reply-lost sidecar operation or event failure can leave
Context ahead of task/audit state without rollback or `recovery_required`.
Separately, `CoreAuthority::checkpoint()` runs
`ContextEngine::maintain(Checkpoint)` and publishes its events after mutation,
although CorePort and RuntimeServices declare that Context scheduling belongs
to Runtime. A checkpoint assembly retry can repeat that maintenance.

Runtime must own both schedules and commit them through a portable Context
checkpoint/restore or an idempotent prepare/commit protocol. Once application
may have occurred, an unprovable reply or audit failure must rollback or enter a
durable recovery fence before any further mutation. Exit tests inject failure
after ingest, after the accepted event, during maintenance event publication,
and between sidecar apply/reply; repeated checkpoint assembly must not repeat
logical maintenance. This restores the intended CorePort boundary rather than
moving more orchestration into Core.

Re-audit record (2026-09-03, local Windows run on `e357bed`, the repaired
transaction plus the actor regression tranche): the M10-facing suites are
green — agent-runtime unit 324, actor 59 (including the turn-start/checkpoint
fault-injection and rollback-fencing scenarios in `tests/actor/context_commit.rs`
and the scratch-state restore validation in `tests/actor/restore.rs`), turn 105,
instance 30, host 32, approval 4, recall 3; agent-replay 52 (barrier location,
seq-gap detection, deterministic context rebuild, restore-vs-full-rebuild
consistency); context-simple 250 (checkpoint restore validation); agent-core
149 + 12; agent-storage 15; agent-workspace 98 with `test-faults` enabled.
Zero failures across all suites; no Context/GC/retrieval/packing surface was
retuned for this record.

#### DURABILITY-BARRIER-01 — make every advertised durable barrier recoverable (**closed on `f055e39`**)

Two storage paths currently overstate their recovery guarantee:

- `FileEventJournal::flush()` only flushes the Rust buffer and never calls
  `sync_data`/`sync_all`, while startup/turn/task `RuntimeCommitBarrier` records
  are treated as durable truth;
- operation-WAL compaction can serialize more than 65,536 live operation
  snapshots, switch metadata to that unreadable generation, delete the old WAL,
  and only then let the next append reject the still-over-limit sequence.

Either narrow the documented durability model explicitly to process-crash
visibility or fsync the file and required directory metadata before publishing
the acknowledgement; the current contract requires the latter. Compaction must
preflight the output record count/bytes before switching metadata and retain the
last readable generation on every failure. Exit evidence includes reopen and
fault-injection tests plus a saturated 65,536-distinct-operation recovery case.

#### PROCESS-AUTHORITY-BOUND-01 — bind execution grants to the executable world (**closed on `f460558` + seal-recheck regression `13cf6c1`)

`ExecArgv` authority covers only the submitted program string and argv prefix.
Actual execution also depends on cwd-based/PATH resolution and caller-supplied
environment (`RUSTC_WRAPPER`, loader variables, language runtime hooks, and
similar controls). A standing grant for a familiar command can therefore run a
workspace-shadowed executable or materially different program under the same
approved identity. The same gap reaches `process.session`.

Before approval, resolve and bind a canonical executable identity, cwd scope
and the security-relevant environment (or disallow mutable cwd/env for standing
reuse); dispatch must prove its actual spawn is a subset of that immutable
bound. Exit tests cover workspace/PATH shadowing, cwd drift, wrapper/loader
variables, symlink replacement and session start. `ToolSpec` remains
model-visible schema and cannot authorize any of these fields.

#### SANDBOX-ATTEST-TRUNCATE-01 — do not over-attest old Landlock ABIs (**closed on `e5e712f`; Linux CI path exercised**)

The Linux adapter falls back to Landlock ABI 1/2, where
`LANDLOCK_ACCESS_FS_TRUNCATE` is unavailable, but reports
`fs_write_confined=true` whenever the ruleset was applied. `Restricted` trusts
that boolean. On those kernels, a truncate/O_TRUNC path outside the allowed
roots is not covered by the claimed write floor.

Require an ABI that can enforce every write operation represented by the flag,
or leave the flag false and make `Restricted` fail closed. Add real-child tests
for create, overwrite, truncate, rename and unlink outside roots at each
supported ABI. The clean-tree M13 artifact remains immutable evidence; it does
not authorize a stronger claim than the kernel mechanism supplied.

#### PROCESS-SESSION-LIFECYCLE-01 — enforce cancel, capacity and kill-then-reap (**closed on `64607f6`**)

`process.session` start does not consult its cancellation token, and the
capacity check is separated from insertion, so concurrent starts can exceed the
declared maximum. Sessions live only in the dispatcher map; the dispatcher/tool
module has no explicit shutdown hook, leaving graceful teardown to direct-child
drop rather than the promised process-tree kill and reap. Adjacent one-shot
process paths can also return through artifact I/O failure after spawn without
the same explicit cleanup.

Reserve capacity atomically before spawn, select cancellation through the full
start, and centralize every post-spawn exit in a bounded kill-tree/reap guard.
Module shutdown must drain all sessions. Exit tests race starts at the cap,
cancel during spawn and artifact failure, and prove both child and descendant
processes are gone after cancellation and normal runtime shutdown.

#### M15-RAW-EVIDENCE-01 — derive the formal verdict from content-addressed raw cells (**closed on `ea821bb`; fail-closed mutation regression included**)

`m15_report` says it reconstructs raw facts, but currently reads only the
window manifest plus per-cell `dimensions.json` and summary. It neither reads
nor hashes `events.jsonl`, hidden verification records or `workspace.json`, and
the window manifest stores paths without cell/file digests. A coordinated edit
to dimensions/summary, or drift between raw events and those projections, can
therefore regenerate a formally valid PASS. Claims that the reporter detects
terminal/event gaps and rebuilds event-derived summaries are not implemented.

Define one content-addressed cell manifest over every acceptance input, parse
the bounded raw streams, derive the dimensions/summary, and compare the derived
record with the stored projection. The window manifest must bind those cell
digests. Mutation tests for every raw file and every derived file must fail
closed. Historical FAIL directories remain immutable diagnostics; no formal
M15 PASS or closure claim is allowed until this reporter proof is real.

### New P1 — replaceability, boundedness and process supervision

#### CONTEXT-MATERIALIZATION-VALIDATE-01 — validate adapters before provider execution (**closed on `d9807e7`**)

`MaterializedContext::validate_requirement_status()` validates only required
ids/misses. It does not bound or cross-check selected/foreground entries,
content/reason sizes or all acknowledgement ids. Runtime sends that result to
the provider before Core validates the consumption ACK. The shipped
`AppendOnlyEngine` ignores `max_selected_items`; 257 short history items can
produce a successful model result that is discarded only when the ACK rejects
the oversized id list. Foreground ids themselves have no ACK count limit.

Add one complete materialization validator at the adapter trust boundary and
run it before durable `ContextPrepared` publication or provider execution.
Validate counts, bytes/tokens, uniqueness, ownership, semantic eligibility,
required/selected/foreground relationships and the eventual ACK envelope.
Baselines must honor the same query caps. Exit tests use hostile adapters and a
257-item append-only actor round.

#### CONTEXT-STORE-TRUTH-01 — verify every blob and run startup reconcile (**closed on `1ea671f`**)

Normal fetch/admit/GC-recall calls use `read_item_async(..., checksum=None)`
even though the external-map owner stores `blob_checksum`. A valid JSON blob
with the same id but changed body can therefore enter the model or resident
heap. The ContextEngine contract also requires the composition root to call
`reconcile_store()` after restore/start, but the product composition does not.
When reconcile quarantines an owned checksum mismatch, it leaves the external
map entry advertising the now-moved blob.

Verify the owner checksum and shape on every authority-bearing read (with
measurement before any cache optimization), invoke reconcile in the production
restore/start transaction, and atomically invalidate or quarantine the owner
entry with its blob. Exit tests cover tampered fetch/admit/recall, orphan and
stale blobs, sidecar parity, and restart with a quarantined owned blob.

#### CONTEXT-CONSUMPTION-TRUTH-01 — acknowledge only the final visible frame (**closed on `7a8a663`**)

Selection exposure is recorded during materialization, before Runtime's final
packing can remove items. Foreground-only bodies are not stamped as consumed;
selected/foreground duplicates can be labelled `BudgetExcluded` when one copy
remains visible; and foreground clipping keeps the original path/revision
identity without a truncation marker. These paths can create false completion
misses and misclassify later rereads as previously selected or GC-induced.

Build the ACK and miss ledger from the exact final rendered frame. Deduplicate
body identity across selected/foreground layers, stamp every body the model
actually consumed, and represent a clipped foreground body as an explicit
partial artifact that cannot stand for the full revision. Do not change GC
thresholds or selection scoring. Exit tests cover final trim, foreground-only
consumption, duplicate required bodies and foreground truncation.

#### CONTEXT-RESOURCE-BOUND-01 — bound reports and GC work at allocation time (**closed on `cfc17a3`**)

Several advertised bounds are applied only after work has grown: maintenance,
GC and reconcile DTO vectors can contain a full scan; storage GC spawns one task
per candidate before its I/O semaphore; full GC clones the pending item/byte
map; failed externalization can return the whole overflow set to a warm buffer
without reapplying capacity; and one hot external entity bucket is cloned and
sorted before the 32-row view limit. These are memory/concurrency bounds, not a
request to retune policy.

Use bounded collectors, pre-spawn work queues/semaphores, references instead of
large item clones, and a lossless spill/retry representation that never exceeds
the hot-buffer cap. Reports must include truncated counts/digests and reasons.
Stress tests need large candidate/entity sets and persistent store failures.

#### INPUT-STATE-BOUND-01 — align live input, task, event and replay limits (**closed on `ab27f86`**)

Live input validates a 240-character preview but has no full-body byte cap and
writes the body artifact unchanged. Replay advertises 256 KiB, yet reads the
whole file before checking; without an artifact workspace, a truncated preview
is silently replayed as the full input. Separately, task creation stores a raw
goal before applying the 2,000-character `TaskAnchor` bound, so an oversized
first message/focus is committed and only later makes every checkpoint fail.
Task/completed catalogs have no count bound, and `FocusChanged.goal` plus
`Pinned.content` have no event byte bound; eventual 16 MiB checkpoint rejection
can make a long-lived runtime permanently non-resumable.

Apply one byte/character policy before live acceptance, artifact write, task
commit and event publication. Replay must stream under the same bound and fail
closed when a preview does not cover a missing body. Normalize every anchor
before any state/event mutation, paginate/bound task catalogs, and define a
recoverable overflow representation. Exit tests cover 256 KiB + 1, no-workspace
truncation, 2,001-character implicit/explicit focus, event-size overflow and
task-catalog saturation.

#### PROCESS-COORDINATOR-01 — use the shared bounded process transport (**closed on `43eb87b`**)

`ProcessEffectBroker` and `broker_host` use blocking `std::process` pipes and
`read_line`, then check 64 KiB only after the full line is allocated. EOF after
JSON is accepted without a frame terminator, RPC has no deadline/cancellation,
and `Drop` can block forever in `child.wait()`. This bypasses the bounded framed
codec and lifecycle controls already owned by `agent-process`; the platform
security document's “bounded frames” claim is therefore false today.

Move the coordinator behind the shared framed process facility (or an equally
bounded narrow adapter), enforce the cap while reading, require complete frame
boundaries, and make request/cancel/shutdown/kill/reap deadlines explicit.
Oversize, partial EOF, stalled peer, malformed session and stubborn-child tests
must all fail closed.

#### CAPABILITY-PROCESS-LIFECYCLE-01 — serialize activation with native host state (**closed on `abcb4ba`**)

Disabling or quarantining a capability clears its model surface but does not
stop an already serving child. `set_activation` also bypasses the per-entry
`run_lock`, so it can race `ensure_started`. After a native invoke poisons the
host, registry state can remain `Started`; later `ensure_started` fast-returns,
so the replacement path and `RestartCircuit` are unreachable. A quarantined
capability may consequently keep running while the registry says it is absent.

Make activation change one lifecycle transaction under the same lock: revoke
leases/surface, cancel inflight work, stop and reap the child, then publish the
new generation. Poisoned invoke must transition to a restartable/quarantined
state and consume the circuit exactly once per replacement attempt. Add
disable-vs-start races, inflight quarantine and invoke-poison recovery tests.

#### EXTENSION-PROCESS-HYGIENE-01 — bound every control-plane send and teardown (**closed on `ebe02ff`**)

The MCP request path sends its frame before entering the timeout/cancellation
select; cancellation notification can also block on a peer that stopped
reading. `ProcessHost` performs a bounded shutdown call followed by unbounded
supervisor reap, and its broker handler awaits broker work without selecting
the request cancellation. The dormant plugin self-check uses raw `Command`,
waits for exit before draining pipes, ignores some timeout-kill failures, lacks
the normal sandbox/attestation path, and its env-scrub test sets one variable
while the helper reads another.

Every send, broker wait, pipe drain, shutdown and reap needs one end-to-end
deadline/cancellation budget and a kill-tree fallback. If plugin self-check is
activated, route it through the normal ProcessHost containment floor and repair
the test oracle first. Backpressure, full-pipe, acknowledged-but-still-alive,
cancelled-broker and secret-variable tests are required.

#### DEPENDENCY-CONFORMANCE-01 — enforce the declared layer graph, not seven pairs (**closed on `2436249`**)

The shipped graph contains `agent-workspace -> agent-process` for process
journal identity/kill helpers, while the contract and architecture diagram
present them as siblings. The edge is not currently forbidden, but it is an
undocumented architectural choice. `agent-conformance` uses a hard-coded
seven-pair denylist and hard-coded role lists, so it cannot detect this drift,
new implementation crates, or the semantic prohibition on
`tool-runtime -> ContextEngine` (the trait lives in the otherwise legal
`agent-contracts` crate).

Decide the narrow edge explicitly: invert it behind a workspace-owned process
control port, or document and constrain the exact dependency. Then replace the
partial guard with an allowed-layer/role matrix plus a source/API-level check
for forbidden ContextEngine use. New production crates must fail until assigned
a role. Keep test-only composition exceptions explicit and non-transitive.

#### M15-HARNESS-BOUNDARY-01 — make cell execution bounded and failure-monotone (**closed on `f57a118`**)

The workspace hash recursively follows ordinary directory classification,
reads every file in full, and excludes only `.focus-agent`; it can traverse
links/junctions outside the fixture and includes `.git`/`target`, unlike the
bounded allowed-diff domain. Oracle timeout wraps `Command::output()` without
owning a kill/reap guard, so timed-out cargo descendants may continue mutating
the workspace. If an earlier model/runtime failure is present, a later harness
setup/watchdog failure is recorded only when the old failure slot is empty,
which can downgrade the required `NOT_RUN` censor to behavior `FAIL`.

Use one canonical, link-safe, file/byte-bounded workspace domain for diff and
hash; persist its file-set digest. Kill-tree/reap oracle children before verdict
or hashing. Failure classification must be monotone: any harness failure makes
the cell `NOT_RUN` regardless of an earlier behavior failure. Add symlink,
`.git`/`target`, large-file, timeout-descendant and dual-failure tests.

### P2 / governance observations from this review

- The README still presents `CONTEXT_RUNTIME_TODO.md` as a live queue, calls
  ROADMAP a general status authority, and describes landed checkpoint/effect
  mechanisms as wholly open. `GOV-STATUS-01` owns the correction.
- The architecture text overstates composition equivalence: product/TUI and
  product-equivalent M15 paths use `agent-compose`, while isolated evaluator
  harnesses deliberately build narrowed `RuntimeServices` directly. Those
  harnesses must not claim product equivalence unless they use the product root.
- `ModuleHost` does not reject duplicate start or unstarted
  `RuntimeInstance::spawn`; `COMPOSE-LIFECYCLE-01` must include those guards and
  post-start rollback.
- `context-simple::materialize` performs I/O between state-lock phases without
  the engine operation gate; direct restore replaces live state before final
  validation; external scope promotion does not dirty the catalog; and ledger
  export removes rows before its fallible write. These are transactional P2
  follow-ups under `CONTEXT-IO-01`; fix them without changing policy scores.
- The TUI task/grant notice path uses an unbounded channel, and numerous code
  comments still carry milestone/defect ids contrary to the repository
  maintenance rule. Bound the channel and remove tracking vocabulary when each
  affected module is next touched; neither justifies Context/GC changes.

### P0 — restore a trustworthy baseline

#### BASELINE-01 — make the exact source green (**closed 2026-08-30 on `1455795`**)

Scope is formatting, warnings and real regressions exposed by those checks;
do not hide an algorithm change in this item. Exit requires all of:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace --all-targets
cargo test --workspace --all-targets
Ubuntu CI PASS
Windows CI PASS
clean worktree at the recorded source
```

The full workspace test must complete; an external timeout is not a pass.

Closure record (2026-08-30). The recorded source is `1455795`; its Rust tree
is commit `8558886` (the last commit changed only `ci.yml`). All six exits
are banked on that source:

- the four local commands are green on the Rust tree: fmt check, all-target
  all-feature Clippy with warnings denied, all-target build, and the complete
  all-target workspace suite;
- Ubuntu and Windows CI both PASS one complete run — check (fmt/clippy/build)
  in under 3 minutes per OS, then the test jobs: Ubuntu as two halves in
  separate fresh-VM jobs (3m09s + 4m49s) and Windows as the full workspace
  (15m01s);
- the worktree is clean at the recorded source.

The two earlier unanswered Ubuntu exits were a hosted-runner
loss-of-communication at ~48 minutes of job wall time (48m02s / 47m59s from
job start, tested step in progress, zero test-level failures, under both full
and capped concurrency) — a wall-clock termination, not a test result. The
full suite cannot fit in one job on that runner, so CI runs the Ubuntu suite
as two parts in separate jobs; the scoped `cargo test -p` jobs additionally
needed three fixture repairs the all-members workspace layout had masked:
`tool-runtime` persist tests now spawn their sleeper as a Unix
process-group leader to honor the group-kill contract (a non-leader child is
invisible to `kill(-pid)` and survives), part 2 explicitly builds
`agent-process`'s `mock_host`/`broker_host` bins and the
`agent-context-service` binary (whose mtime freshness guard is tripped by
warm-cache restore in a fresh checkout), and both those jobs rebuild and
stamp the service binary accordingly.

Current successor note (2026-09-03): the context-service-specific workaround
above describes the recorded 2026-08-30 source and is no longer the live test
contract. The real process-boundary suite is now an integration target owned
by `agent-context-service` and executes Cargo's exact `CARGO_BIN_EXE` binary;
no mtime guard, explicit service build, or timestamp touch remains. The scoped
`agent-process` fixture build is still required for its own sibling binaries.

### P0 — one completion authority

#### COMPLETE-AUTH-01 — derive one bounded `CompletionReadiness` (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-runtime/src/task.rs`,
`crates/agent-runtime/src/actor/{turn,model}.rs` and the bounded
contracts/events they consume.

Root cause: settlement uses the strong task-aware `task_ready` join, while
`task.complete` and durable commit use a weaker gate. They can therefore
disagree about whether the same task is complete.

Implement one Runtime-owned, pure derived result and use it for settlement
label/projection, completion proposal acceptance and durable completion. The
derivation accepts `CompletionIntent = ModelProposal | ExplicitOperator`; this
is one typed decision surface, not two hidden gates. Keep
two orthogonal bases: `(task_id, anchor.revision)` synchronizes task/execution
state, while `(task_id, anchor.verification_revision, directive_revision,
workspace_revision)` decides verification currentness. A progress-only CAS may
move the first and be synchronized without staling proof on the second. The
result returns bounded typed refusal reasons and includes:

```text
current trusted verification PASS satisfies the task-declared identity strength
(ExactCurrentWorld where that profile requires it)
+ criterion-addressed current acceptance receipts
+ no open loop or next action
+ no in-flight/cancel cleanup
+ no actor recovery fence
+ no unresolved failure or execution obligation
+ no hard required-context miss
```

The result separates `task_state_current`, `commit_safe` (valid authorized
intent, active matching task, no recovery/in-flight/unresolved effect
transaction) and `verified_ready` (all semantic/evidence rows above).
Settlement and model `task.complete` require all three. Explicit user/host
closure may bypass only `verified_ready`; it still requires current task state
and `commit_safe`, persists a typed `OperatorOverride` disposition plus bounded
unmet semantic reasons, and never fabricates a verified-success record. Future
ACK debt from `EFFECT-ACK-01` joins `commit_safe` rather than creating another
completion gate.

Ordinary assistant final remains a separate turn boundary. Do not auto-close,
add a fixed-round stop, move task authority into Core/Context, or add completion
pressure to the prompt. Also make `TaskProgressView::is_empty()` account for
`stall_warning`, `frontier_warning` and `completion_opportunity` so an advisory-
only bounded view is not silently dropped.

Exit tests must show the settlement predicate and `task.complete` decision are
identical for positive and negative matrices: missing acceptance, mismatched
coverage, stale verification, new directive, open loop, next action, unrelated
failure, recovery fence, in-flight cleanup and hard required-context miss.
Accepted completion remains one-shot, while ordinary final may end a turn
without fabricating durable task closure.

#### CONTINUATION-EPOCH-01 — continuation is not a new directive (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-runtime/src/actor/turn.rs`,
`crates/agent-runtime/src/task.rs` and
`crates/agent-runtime/src/execution/state.rs`.

`continue_active_task_turn` currently enters `begin_applied_turn`, whose
unconditional `on_user_turn` advances `directive_revision`. This invalidates an
otherwise current verification even though the stored user instruction did not
change.

- `TaskContinuation` preserves task/directive/verification identity;
- only new user dialogue advances the directive revision;
- a real mutation, failure or boundary change after continuation still
  invalidates old proof;
- checkpoint/restore preserves the same rule.

Add tuple assertions for continuation, new instruction, continuation followed
by mutation, and cold restore. Existing anchor-only assertions are insufficient.

#### ACCEPT-RECEIPT-01 — criterion-addressed acceptance evidence (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-runtime/src/actor/tools.rs`,
`crates/agent-runtime/src/task.rs`, execution facts, events and checkpoint
schema.

The current observation path binds one pre-dispatch verifier identity to every
criterion, before knowing whether the verification succeeded. Production tasks
may also have no declared criteria. First add an authoritative, bounded
task-creation/anchor-ingestion path; the model-routable `task.manage` must not
mint its own acceptance authority. Each task carries an explicit completion
policy:

- `EvidenceRequired`: a non-empty set of stable criteria is supplied by the
  user/host and matching receipts are required for model `task.complete`;
- `OperatorClosureOnly`: the conservative default when no criteria were
  supplied. Ordinary final remains available, model `task.complete` is refused,
  and `CompletionIntent::ExplicitOperator` may close only through the shared
  `commit_safe` decision while recording an override rather than fabricated
  verification success.

Replace the current fan-out with bounded receipts:

- in V1 each criterion is addressed by
  `(anchor.verification_revision, criterion_index)` plus a host-declared
  verification/coverage domain; a new permanent string id is not required;
- only an observed successful matching PASS may mint a receipt;
- each receipt binds task, criterion identity, directive revision, workspace
  revision, verification identity, and the exact host coverage-declaration
  revision/source digest;
- coverage changes emit a bounded event and survive checkpoint/replay;
- missing declarations, domains or receipts stop at `VerifiedCurrent`.

Criterion content/order changes advance `TaskAnchor.verification_revision` and
invalidate old receipts. Receipt/coverage mutation advances only the full anchor
CAS revision; it must not advance the verification basis and self-stale the PASS
that just earned it. One actor transaction performs: match the observed PASS to
the declared domain, CAS the receipts, synchronize `execution.anchor_revision`,
append the bounded coverage event, then derive readiness. Append/CAS failure
fences the decision and cannot expose a partially ready task.

Do not infer equivalence from free-form commands and do not expose hidden
oracles to the model. One receipt must not cover an unrelated criterion; a
failed verifier must mint none.

Eval task creation must ingest criteria from each frozen fixture's **public
behavior contract**, not the current generic “behaves correctly” declaration
and not hidden implementation needles. For example, a retry-policy diagnostic
may declare that large attempts saturate at `max_delay`; satisfying it requires
a matching boundary-test/probe receipt, while a broad Cargo PASS alone is
insufficient. Persist the declaration/source digest so this rule generalizes by
semantic domain rather than fixture id.

#### FAILURE-LIFECYCLE-01 — resolve failures by domain (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-runtime/src/execution/freshness.rs` and the
bounded failure/obligation ledger.

A trusted PASS currently clears every `failed_command`. Preserve historical
attempts separately from unresolved blockers and resolve only a matching
identity/domain/resource/obligation. Unrelated failures must survive and keep
completion blocked. This shares the typed coverage vocabulary with
`ACCEPT-RECEIPT-01`; it must not become another string matcher.

#### CONTEXT-REQUIRED-01 — required Context misses are completion-visible (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/context-simple/src/{materializer,store}.rs`, bounded
Context contracts and the Runtime readiness/event join.

The root defect was that required or pinned material could be skipped without
an explicit result while store read collapsed missing, corrupt and I/O failure
to the same `None`. The landed candidate now:

- store reads distinguish `Missing`, `Corrupt` and `IoFailed`;
- materialization returns a bounded `required_misses` list with stable identity
  and reason, including budget exclusion;
- Runtime publishes a bounded degradation reason and treats a hard miss as not
  ready;
- optional Context misses remain observational and do not block completion;
- required overlay is transactional per body: it plans largest-first optional
  displacement without mutation, commits only when the body can satisfy both
  token and item bounds, and otherwise records `BudgetExcluded` without
  damaging the already useful frame.

Exit covers budget exclusion, absent artifact, corrupt artifact, store I/O
failure, cold reread success and bounded truncation of the miss list. This is a
correctness signal only; it does not retune selection or GC.

### P0 — restore causal and replay truth

#### EVAL-CAUSAL-01 — isolate settlement as the only treatment (**conditional — same-state audit locally green; common-prefix fork required only for a settlement-enabled candidate**)

Primary code: `crates/agent-runtime/src/services.rs`,
`crates/agent-runtime/src/actor/{model,lifecycle}.rs` and
`crates/agent-eval/src/long_live.rs`.

Replace the overloaded eval switch with independent configuration:

```text
project_task_progress = true       # identical product surface in both arms
project_settlement    = false|true # the only treatment
```

Before the split, the product default turned `project_task_progress` on and
consequently also enabled settlement, while the eval default removed both. The
implemented independent switches now express one common baseline.
The treatment must not alter the TaskProgress body, Context query/materialized
set, checked-file GC projection, tool surface, host policy or provider tuple.
Product and formal-M15 defaults keep TaskProgress on and settlement off unless
a later independent candidate gate promotes it.

Before any live pair, a deterministic same-state preflight must compare the two
assembled requests and prove that the only structured difference is the
settlement node. This assertion applies before behavior diverges; it does not
require live prompts from divergent trajectories to stay byte-identical.
Persist both switch values plus source, pack, fixture, surface, prompt and
provider/config digests. Any other preflight difference is
`INVALID_ARM_DIFF`.

The product hot path does not use the diagnostic envelope: settlement-off and
settlement-on product requests pack only the arm they actually send, without a
second assembled/cloned `ModelInput` or request-audit hashing. The explicitly
enabled causal diagnostic alone uses the common treatment-sized envelope. A
live off/on claim is still invalid until both arms fork from one pre-exposure
durable Runtime checkpoint and one byte-identical workspace snapshot, preserve
opaque runtime identities, and pin an explicit provider protocol. Do not
alpha-normalize independently minted ids to manufacture equality.

#### EVAL-EPISODE-PAIR-01 — bounded episodes and explicit pair joins (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-eval/src/{metrics,long_live}.rs` and its report
path.

- close an episode on reopening, `TaskCompleted`, ordinary-final
  `TurnCompleted`, new-user boundary or continuation boundary, and record the
  terminal mechanism;
- join arms by stable logical identity
  `(candidate/source, pack/fixture, mode, repeat, acceptance-domain revision/source,
  provider-config digest)`;
  runtime `TaskId` is cell provenance and is not a pair key;
- reject missing, duplicate or mismatched cells rather than sorting and
  `zip`-pairing them;
- for two repeats, report both observations and their midpoint; do not label an
  upper-nearest order statistic as an ordinary median;
- diagnostic marker-shape counts remain observational unless prospectively
  frozen against a behavior oracle.

Terminal attribution follows the real actor order: a committed
`TurnCompleted` remains pending through the following event tail and becomes
`TaskCompleted` only when the matching `RuntimeCommitBarrier(TaskCompletion)`
lands; otherwise the bounded quiet/trace boundary closes it as an ordinary
turn. Tests must use this actor order, not a synthetic `TaskCompleted`-first
trace.

The old convergence bundle stays mechanically reproducible, but its settlement
causal conclusion is superseded by this item.

#### TERMINAL-COMMIT-01 — make task closure one recoverable transaction (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Model `task.complete` first records `pending_terminal_commit`. Runtime prepares
rollbackable post-completion Context, freezes the prospective terminal
task/focus plane in checkpoint v4 (`event_cover_seq` and `terminal_commit` are
serde-defaulted), durably acknowledges it, then performs only infallible actor
assignments. `TaskCompleted`, maintenance events and
`RuntimeCommitBarrier(TaskCompletion)` form one bounded durable batch.
Pre-checkpoint failure leaves the task active and records bounded
`CompletionCommitFailed`; post-checkpoint audit failure keeps completion
authoritative and recovery-fences the runtime.

#### STARTUP-COMMIT-01 — enter service only after the run marker commits (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

The actor owns a one-shot `NotStarted | Serving | StartFailed` lifecycle. Only
a successfully appended and flushed `RunStarted +
RuntimeCommitBarrier(RunStart)` batch enters `Serving`. Pre-start mutations and
duplicate starts are rejected. Any startup append/flush failure permanently
recovery-fences that actor: it cannot retry the marker or accept work, and
shutdown performs no `RunCompleted` append or flush that could accidentally
commit the forensic startup prefix. Read-only task/context/operation inspection
remains available. Exit tests cover pre-start mutation, duplicate start,
second-append failure, flush failure and shutdown after failed startup.

#### REPLAY-COMMITTED-01 — rebuild Context from the committed prefix (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/agent-replay/src/recovery.rs` and
`crates/agent-replay/src/lib.rs`.

If a trace contains any `RuntimeCommitBarrier`, only those markers advance the
committed prefix. New runs durably write `RuntimeCommitBarrier(RunStart)` before
work so a partially appended first turn cannot be mistaken for a legacy trace.
Only a trace with no explicit marker may use bare `TurnCompleted` as a legacy
fallback. Split the stream explicitly:

```text
committed_prefix = sequence <= last_committed_sequence
forensic_suffix  = sequence >  last_committed_sequence
```

Context rebuild and restore-consistency truth consume only the prefix. The
suffix remains available for batch/effect/failure diagnostics. This is a replay
and audit-truth defect; do not overstate it as proof that the checkpoint-based
production restore path already resurrects the suffix.

The repository-wide review adds three fail-closed requirements to this item.
File entry points must bound bytes/lines/events while reading and reject a
mixed-run stream instead of silently retaining only the first run id.
`verify_restore_consistency` must reject gaps, duplicates and out-of-order
sequences before comparing two reconstructions; equality over the same damaged
prefix is not consistency proof. A no-artifact truncated user-input preview is
also unreplayable and must fail rather than silently replace the original body.

For a terminal checkpoint, a matching `TaskCompletion` barrier advances trace
truth to that transaction. In the checkpoint-to-audit crash window, the
validated terminal checkpoint is the stronger truth; `event_cover_seq` is a
replay cursor, not commit authorization.

Tests cover no committed turn, one uncommitted tail, several committed turns
plus a final failed turn, suffix-only warnings/errors, mixed-run input, byte and
event overflow, missing/duplicate/out-of-order sequence rows, and missing
truncated bodies. Gap/interruption diagnostics remain visible, but any gap makes
restore-consistency proof invalid.

#### PROVIDER-PROTOCOL-01 — fail closed before new live evidence (**closed 2026-09-03 on `97a7719`; local gate green, dual-platform CI run `33709924715` green**)

Primary code: `crates/provider-openai/src/{lib,sse,responses,retry}.rs`.

Valid SSE `data:` with malformed JSON is a typed protocol error, not a skipped
line; incomplete tool arguments are `MalformedToolCall`, not `null`. Unknown
but valid extension events may be ignored. Interactive/live sinks never replay
after publishing content; buffering eval sinks may retry only before publish.
Use bounded `Retry-After`, checked/saturating backoff and injectable
deterministic jitter.

### P1 — reduce calls before the settlement tail

#### VERIFY-ROUTE-01 — make verifier eligibility explicit (**closed 2026-08-30 on the deterministic verify-route gate**)

The latest evidence often reaches a verifier quickly but sometimes selects a
TaskScoped recipe that cannot establish an exact current-world PASS. This is a
pre-settlement routing problem, not evidence that the settlement hint reduces
work.

Recipes declare a host-owned coverage domain, identity strength and declaration
revision; criteria declare the minimum domain/strength. The model-visible
schema says which recipe class can satisfy the requirement. Equivalence is
host-declared, never inferred from argv. Reuse is allowed only on the same
current world and domain.

Measure decisions/tool calls to the first **criterion-satisfying** trusted PASS,
recipe-choice rate, duplicate verification, discovery/load calls,
pre-settlement rounds/calls and survival of unrelated failures. Include a
negative in which a broad Cargo/test PASS is valid evidence but does not cover a
criterion-specific boundary condition, plus a positive boundary receipt for the
same public criterion. Context/GC/retrieval/packing remain frozen.

Closure record (2026-08-30). Slice A makes eligibility model-visible: the
`verify.run` schema catalog marks every recipe's identity class
(`[task-scoped]` / `[exact current-world]`) plus its declared coverage domain,
and the acceptance-criterion view line names the required domain, so the model
can pick a recipe class that can satisfy the criterion. Slice B adds the
deterministic verify-route gate (`agent-eval --verify-route-gate`; evidence
under `crates/agent-eval/evidence/verify-route/`), three cells over the real
runtime/tool surface:

- negative: a broad `cargo test` PASS records valid evidence but never mints a
  receipt, `task.complete` is refused ("lack current coverage"), and only the
  declared exact recipe closes the task (calls-to-first-satisfying = 3);
- positive: the first exact PASS satisfies the criterion
  (calls-to-first-satisfying = 1), an identical repeat reuses the recorded
  PASS without a second process, and completion is accepted;
- unrelated failure survival: a failed `process.run` survives the exact PASS;
  the receipt mints but completion stays refused by the
  unresolved-failed-command gate.

The four local commands pass on the closing commit `7ee56e8`, and the
dual-platform CI run `33305302134` is green (Ubuntu parts 1+2 and Windows
full, check job included). No Context/GC/retrieval/packing path
changed.

#### FAILURE-SPILL-01 — retire overflowed blockers exactly in long tasks (**open P1; activate before long-horizon evidence or after measured overflow**)

The current bounded hot sets retain at most 32 exact obligations and 32 exact
failed commands; overflow becomes checkpointed body-free count/digest debt and
therefore remains safely completion-blocking. For long-horizon liveness, store
overflow identities once behind a task-owned artifact/ref and keep only its
digest/count/ref hot. A matching success may retire exact spilled rows;
missing/corrupt spill stays fail-closed. Never expand TaskProgress or the
transcript, and do not build this path before measurement shows it is exercised.

### P1 — V1 correctness hardening

Every landed slice named below is part of the `4e56f69` code identity whose
local source gate and dual-platform CI run `33663057012` are recorded in
[`STATUS.md`](STATUS.md). An **open** heading now names only its explicit
residual; it does not mean the landed slice is absent.

#### COMPOSE-LIFECYCLE-01 — make startup transactional (**open — transaction body landed on `e566615`; optional-service type residual remains**)

`e566615` moved fallible journal/authority/service preparation before
`host.start()`, rolls the sole post-start reconcile seam back, rejects duplicate
`ModuleHost::start`, and refuses `RuntimeInstance::spawn` over an unstarted host.
The failure and lifecycle tests cover locked preparation, post-start rollback,
module-start rollback, duplicate start and the serving assertion; narrowed eval
compositions are labelled non-product-equivalent.

The remaining defect is typed optional lookup. `ServiceRegistry::event_store()`
and `artifact_store()` currently turn every `get()` error into `None`, so a
registered capability with the wrong concrete type is indistinguishable from an
absent optional service. Preserve absence as optional, but return an error for a
present wrong type. Exit adds both positive absence and negative wrong-type
tests without weakening the existing startup rollback matrix.

#### CONTEXT-IO-01 — remove lock-across-I/O and loss-on-export (**closed 2026-09-04**)

`c7ed011` plans external `Admit` under the state lock, performs the checked
store read outside it, revalidates on commit, and merges bounded lifecycle rows
back after a failed ledger write/rename. The slow-read and failed-export tests
prove those two original failures. GC, storage GC and store reconcile already
use the engine operation gate around plan/I/O/commit, and restore validates a
scratch replacement before committing it.

The residual is closed without changing Context selection or GC scoring.
Stored-body materialization now holds the existing operation gate across
plan/I/O/preview commit; a controlled restore race proves restore waits, clears
the prior pending preview, and rejects its stale acknowledgement. Restore keeps
the process-lifetime maximum materialization revision, preventing preview-id
ABA after rollback. Every state-changing engine API now shares the gate,
including ingest/maintenance/scope changes, access-stamping retrieval, ledger
export, Admit/Fetch and the existing GC/reconcile/checkpoint/restore paths; no
lifecycle mutation can cross another operation's unlocked I/O window. Scope
promotion marks one catalog rebuild for the whole external batch, while
supersession/verification mark their changed ids; seeded search/inspect
regressions prove new scope/label/live/attention keys and removal of the old
buckets. Focused lifecycle/search/GC regressions cover these boundaries without
changing policy thresholds or scores.

#### EFFECT-ACK-01 — persist unresolved ACK debt (**open — typed debt/event/fence landed on `245b2a6`; durable lifecycle residual remains**)

`245b2a6` preserves the real typed receipt when broker acknowledgement fails,
emits bounded `EffectAckDebt`, enters the Runtime recovery fence and refuses
later mutation. Existing tests cover the truthful Applied debt, event and live
fence, while the broker journal truthfully reopens dispatched-without-ACK as
ambiguous rather than strengthening it.

The debt itself is not yet a checkpointed lifecycle: `RuntimeCheckpoint` carries
no unresolved ACK-debt set, there is no typed resolved event/operation, and the
restart matrix does not resolve Applied/NotApplied/Ambiguous debts end to end.
Add the bounded checkpoint/run-status projection and explicit reconciliation
path before closing this item. Never pretend an already-applied effect rolled
back, and never serialize the typed settlement as a boolean.

#### DURABLE-FACTS-01 — make typed execution facts replayable (**closed on `c0c1c5c`**)

`ToolFinished` now carries a versioned top-level `ExecutionFactsEnvelope`.
Runtime stamps it from trusted native facts; replay validates and prefers it,
retains the reserved-metadata fallback for legacy events, and fails closed on
an unknown envelope version. `TaskCompleted` replay uses the event's real
`task_id`. Tests prove top-level precedence, legacy fallback, old/new semantic
equivalence, invalid-version refusal and task completion identity. The fallback
may be removed only under a separately versioned migration.

#### SIDECAR-ERROR-01 — preserve error categories across process boundaries (**open — framing envelope landed on `726f2a5`; semantic parity residual remains**)

`726f2a5` introduced a bounded `ServiceErrorEnvelope` with category,
retryability and message, and makes terminal framing/protocol failures exit
non-zero. Malformed JSON/UTF-8/EOF/version/budget cases and clean EOF are
covered.

Engine semantics still collapse at the boundary: the service currently maps
every `ContextEngine` error to `Engine, retryable=false`, and the adapter wraps
the envelope back into a generic `AgentError::Context` string. Storage,
`RecoveryRequired` and retryability therefore cannot drive the same Runtime
decision as their in-process forms. Exit injects those categories through both
adapters and proves identical typed decisions; bounded diagnostic text remains
non-authoritative.

#### TOOL-MANIFEST-01 — align the actual model surface (**closed: v5 parity `23abe1c`; evaluated-identity persistence wired on `9e00299`**)

`23abe1c` aligned `TOOL_INVENTORY.json` with the actual v5 production surface,
made unknown/uninspectable rows and default-surface drift fail closed, and
computes a stable surface/schema digest. `9e00299` completed the persistence
residual: the derivation moved to `agent_contracts::tool::surface_digest`
(shared by the conformance checks and the evidence writers), and the formal
long-live path records the digest of the exact builtin dispatcher +
frozen-verification-recipes composition it evaluates on every cell
`manifest.json` (`surface_digest`), with a regression proving the manifest
carries it. Surface drift between runs is now detectable from the evidence
alone.

A single host-owned generated manifest may be evaluated later as a separate
post-M15 simplification. It is not required to prove current parity and must not
be mixed with M15 algorithm work.

#### EVAL-ACCOUNT-01 — separate restored protocol from Context cost (**closed on `83cbd60`; actor path proved on `e357bed`**)

Prompt accounting now reports `restored_protocol_tokens` independently and
excludes those rehydrated bodies from `historical_context_tokens`; the event and
eval aggregate retain both fields, with zero-default compatibility for legacy
records. Unit coverage proves the split and aggregation, while the actor-level
checkpointed body regression on `e357bed` proves the real model request and
`ModelUsed` accounting path. Episode terminal boundaries remain owned by
`EVAL-EPISODE-PAIR-01`.

### P2 candidates — not actionable before measurement

- add deterministic no-model benchmarks for Context admit/materialize/GC,
  prompt assembly, tool-surface construction, replay and journal paths;
- cache immutable `ToolSpec` values or optimize `clip_to_token_budget()` only
  after profiling shows a material hotspot;
- split large modules only at stable transaction boundaries after semantics are
  frozen; do not mix the move with behavior changes.

### GOV-MAINT-01 — bounded repository maintenance (**closed on `6fdb4f0`**)

The MIT `LICENSE` and a minimal `inspect_outbound.sh` landed in `6fdb4f0`
(2026-09-02) with dual-platform CI green on that source. No `docs/state.json`
authority source was added.

### GOV-STATUS-01 — reconcile milestone authority (**closed on `bba1c76`**)

`AGENTS.md` previously contained both banked-closure wording ("The platform
gates M12/M13 closed...") and an instruction not to claim M12/M13 closed
("M12 and M13 must still finish..."). The 2026-09-03 edit reconciles both
spots to one statement: mechanism substrate implemented, evidence banked,
closure claims suspended until the M15-facing exits in `docs/STATUS.md` are
recorded, Self-Iteration blocked. README is aligned in the same tranche:
`agent-kernel` renamed to `agent-core`, the crate list now includes
`agent-platform-protocol`/`agent-conformance`/`agent-compose`,
`CONTEXT_RUNTIME_TODO.md` is labeled historical, landed mechanisms are no
longer listed as open, and the authority pointers now name STATUS (Now/
freeze), ROADMAP (gates/order), AUDIT_TODO (defects) and M15_ACCEPTANCE.
Keep the evidence banked, do not reopen its mechanism work, do not claim
closure, and keep Self-Iteration blocked until the remaining exits land.

### Ordered execution and evidence gate

```text
repaired-source + PinAI/Luna candidate REJECTED
  (latest valid FAIL at the time 6/12 on 43e1033; prior 784d7aa FAIL retained)
  -> repository-wide P0 close or exact selected-path exclusion
     [DONE 2026-09-02: all repaired in 615b5ed..6fdb4f0, CI green on 6fdb4f0]
  -> deterministic AttemptIncident / CompletionRepair / JSON replay gates
     [DONE 2026-08-31..09-02: 615b5ed, b148b4d, d92b250, 1768914, 33d0395,
      531a77e, df2f72c and regressions]
  -> close the dfb9ade Responses terminal-state defects + typed retry evidence
     [DONE: 1768914, f528a92, 328ec5d; formal-path/product observer 7e02488]
  -> pre-approval schema validation
     [DONE: 33d0395]
  -> full local regression + recorded clean source + Ubuntu/Windows CI
     [DONE: clean code identity 4e56f69; dual-platform run 33663057012]
  -> M10 fault-gate re-audit on the repaired turn/context transaction
     [DONE 2026-09-03: recorded on e357bed in RUNTIME-CONTEXT-COMMIT-01]
  -> GOV-STATUS-01 reconciliation (AGENTS.md wording + README alignment)
     [DONE: bba1c76]
  -> wire the typed retry observer into long_live/fixture_driver/agent-compose
     [DONE: 7e02488]
  -> same-checkpoint causal fork only if project_settlement changes
     [SKIPPED FOR THIS CANDIDATE: project_settlement=false]
attempt-incident admission candidate (e897c5c)
  -> deterministic attempt-incident/negative-fact matrix + record clean source
     [DONE 2026-09-03: matrix green; clean source 03bc6d5; run 33703472111]
  -> new exact-source/product M15 preflight
     [DONE: relay attempt retained NOT_RUN; PinAI/Luna attempt5 PASS on
      c823a1c; attempt6 PASS on 51559d4]
  -> at most one freshly predeclared formal 12-cell M15 window
     [DONE 2026-09-03: valid FAIL 10/12, 0 NOT_RUN, _windows/1788402676712]
  -> both candidates rejected; preserve the windows and return to bounded
     diagnosis (m15-diagnosis-attempt-incident/REPORT.md)
completion-gate convergence candidate (selected 2026-09-03 by operator
direction; the explicit recommendation of the diagnosis)
  -> deterministic terminal-surface matrix: operator-only / no-resolver
     refusals escalate to an explicit ordinary-final terminal stage after
     consecutive identical-basis refusals, with deferred safe-point refusal
     visible to the next model decision
     [DONE: COMPLETION_REPAIR_TERMINAL_REFUSALS + terminal surface plan;
      completion::operator_only_refusals_escalate_to_a_terminal_surface]
  -> full local gate + recorded clean source + Ubuntu/Windows CI
     [DONE 2026-09-03: local gate green on cc60194 (fmt/build/clippy/full
      suite); dual-platform CI run 33740918365 green]
  -> new exact-source/product M15 preflight
     [DONE 2026-09-03: PASS on clean HEAD 2adad31, one `retry_policy_dev`
      normal cell, task_progress product surface, PinAI tuple, explicit
      protocol; evidence crates/agent-eval/evidence/m15-preflight/]
  -> at most one freshly predeclared formal 12-cell M15 window
     [DONE 2026-09-03: valid FAIL 10/12, 0 NOT_RUN, _windows/1788438275930,
      predeclared clean source a6dc33e]
  -> candidate REJECTED; preserve the window and return to bounded diagnosis
     (m15-diagnosis-completion-gate/REPORT.md)
```

The completion-gate candidate closed the diag tail (diag 4/4) but the two
residual `retry_policy_dev` failures are a distinct execution-debt tail (a
resolvable-looking `failed_commands` blocker on which the ordinary-final
terminal stage is deliberately not offered, ending in 48-round budget
exhaustion) and a Runtime restore/storage lifecycle lock-contention failure.
The latter cell is mechanically classified `runtime`, not as an M15
`harness_setup`/`harness_watchdog` NOT_RUN.
Both are uncovered by the terminal escalation; the candidate is rejected and
M15 stays open. The next bounded M15 candidate is an operator decision bounded
by the frozen route.

Successor-source correction and repair (2026-09-03; no formal rerun): the
immutable stream shows that the same-`argument_digest` successful formatting
check retired its earlier failure. The persistent row was the unrooted
`fs.read src/job.rs` `PathNotFound` observation. Negative-fact admission and
failed-command disposition now share one trusted predicate, so only an exact
unrooted Observe/Search miss avoids completion debt; all rooted/mismatched/
unattributed cases stay conservative. The resume lock was traced to a detached
model future retaining the full service bundle after cancellation; that task
now captures only `ModelTransport`, with a regression that reacquires the same
workspace-effect journal before releasing the lagging future. Supporting
repairs make CLI validation side-effect-free, Python/helper discovery typed and
Cargo-owned, and local buffered capacity distinct from malformed provider
data. The successor local doctor gate is green (pre-recording source-tree digest
`73155555cc8e20cd…`: Python/helper, format, all-target/all-feature check,
strict Clippy, build, complete all-target workspace tests); the Provider probe
was intentionally skipped without a key. These changes do not alter the
rejected source or authorize another window.

The settlement candidate gate, if it is run again after deterministic fixes,
is `normal/resume × off/on × at least two explicitly joined repeats`; only
`project_settlement` may differ. It is optional: a corrected base product with
settlement off may proceed to exact-source M15 preflight. A projection-changing
candidate must pass its own causal paired gate before entering M15.

## Open P0 — trusted execution

The platform closure-audit evidence was banked 2026-08-27
(CORE-01/CORE-12 moved to the archive below), but `GOV-STATUS-01` withholds a
new overall M12/M13 closure claim. Residual OS isolation stays
outside the V1 availability floor — Linux UDP / raw / pathname-Unix, absolute
OS-level reads, Windows OS-level network, I/O bandwidth quotas,
seccomp / AppContainer — and `UntrustedGenerated` keeps failing closed on
native; making that profile runnable through WASI remains a V2 candidate.
Matrix: [`PLATFORM_SECURITY.md`](PLATFORM_SECURITY.md).

### CORE-10 — protocol remaining (not a transport swap)

`PLAT-00`–`PLAT-04` containment/protocol proof is landed. Remaining:

- PLAT-06 multiplexing (stay single-inflight in v0)
- PLAT-07 adapter envelope migration
- PLAT-08 Named Pipe/UDS (later)

Named pipes/UDS are not a fix for the platform gates. V1 still trusts Runtime
in the same address space.

## Open P1 — Tool Surface reliability

### PROV-LINK-01 — retryable transport failures killed runs instead of retrying (fixed 2026-08-26)

Live evidence kept dying on errors the provider layer itself marked
`retryable=true`: relay streams ended in `stream error: error decoding
response body` mid-round, and cells hung for minutes before surfacing it.
Two root causes, both fixed in `provider-openai`. First, the streaming
retry wrapper refused to replay once anything had reached the sink — the
right rule for a live UI listener, but an outcome-measuring harness has no
live listener to corrupt, so its runs had no recovery path at all. The
wrapper now has a buffering mode (`RetryingTransport::new_buffering`):
each attempt's chunks are collected internally and only a successful
attempt is forwarded, so every retryable transport failure replays from
scratch without duplication; the eval driver uses it. Interactive hosts
keep the live mode and its no-replay rule. Second, streamed bodies had no
idle bound: a silent peer held the turn open until the total client
deadline (or the peer) gave up. Both protocol paths now fail retryable
with a named stall error after `config.timeout` without bytes, resetting
on every delivered line. Deterministic coverage: three buffering-mode
tests (mid-stream replay with attempt stamping, non-retryable immediate
surface without leaking buffered output, exhaustion) plus a real-socket
stall test proving the idle bound fires long before the client deadline.

End-to-end validation landed the same day: re-running the OpenCode-relay
gate window after the fix, the cell that previously died on a relay
stream decode error replayed and completed (`strict=11/12 → 12/12`,
`usage_incomplete_cells=1 → 0`, lower-bound tokens gone; two cells carry
multi-minute walls as the visible cost of in-place replay). With
transport noise removed, applied-patch correctness is proven across two
providers and seven windows with zero wrong bytes ever committed, and
every remaining gate violation on either serving is model decision
behavior — chiefly the post-edit confirmation read. See
`tool-surface-edit-v3-clean-tree-2026-08-26-ox-r2/REPORT.md`.

### TOOL-CONTRACT-01 — optional-union and cursor semantics (deterministic fix landed 2026-08-24; live gate open)

The PinAI/Luna long-flow trace attributed 25/29 Dynamic failed outputs to
malformed pagination capabilities. A trial that silently mapped empty/zero
cursors to page one removed those failures but increased C from 75 rounds / 85
calls to 137 / 171 and created a 47-round turn; paired A timed out. A second
trial kept strict execution and published a cursor regex; the model then
fabricated matching-looking artifact identities, all 25 file/search calls
failed, C used 107/112 rounds/calls, and A timed out at turn 6. Neither trial
is accepted.

The retained surface uses `artifact.read` as the sole model-visible spill
continuation. `fs.list`, `search.grep`, and `code.symbols` return bounded first
pages plus a run-owned artifact ref and next line; their snapshot cursors stay
parser-only compatibility and execution remains fail-closed. This removes
three optional opaque-capability fields without adding a tool or prompt state.

The negative run also exposed `context.manage` parsing every property of its
union before `op` dispatch: unused empty UUID/enum fields invalidated valid
fetch/search requests. It now parses only fields consumed by the selected op,
publishes bounded kind/scope enums, and remains strict for required/relevant
values. `tool-runtime` unit tests and clippy are green. This is deterministic
tool-contract correctness, not a live convergence claim. The open gate is at
least two paired repeats with unchanged hidden success, lower median rounds and
calls, and no new p95/max-turn tail. Evidence:
`crates/agent-eval/evidence/longflow-pinai-luna-responses-2026-08-24/REPORT.md`
and `longflow-pinai-luna-cursor-normalized-2026-08-24/REPORT.md`.
The strict-schema follow-up is
`longflow-pinai-luna-tool-contract-2026-08-24/REPORT.md`.

The same trace showed `task.complete` at 16 calls / 5 failures from invalid
model-supplied artifact claims. Runtime already merges its trusted current
assistant artifact and current verification refs into `CompletionRecord`.
The model schema now requests only the bounded summary; the artifact list is
parser-only compatibility and remains strictly validated when a trusted caller
uses it. A follow-up 4/4 pair reduced failures to 2/1 but left C at 77 rounds /
84 calls versus A's 49/36, so this deterministic defect is fixed while the
live round/call acceptance remains open.

### TOOL-CONTINUITY-01 — turn completion must not erase multi-turn task affinity (fixed; the CompletionOpportunity candidate ENDED by its 2026-08-28 decision-grade gate)

The one-shot completion trace isolated a lifecycle feedback loop rather than a
Context-size problem. Dynamic C called and committed `task.complete` on 9/15
turns versus A's 3/15. Each commit closed the task scope; the next user
directive started with a new task id and empty task-scoped `TaskProgress`, then
repeated capability discovery, list/read/search, and another completion.

The ended candidate separated implicit final-answer/turn completion from
durable task closure by making `task.complete` catalog-cold and leasing it for
explicit task-closure intent or a task requirement. Surface rev v5 later made
the compact schema always visible as a separate product decision. In either
surface, an accepted clean completion terminates without a confirmation model
round; failed siblings and invalid verification gates keep the recovery round.
This changes no Context/GC threshold or retrieval score.

Deterministic tool/runtime tests are green. A short live edit passed in 3
rounds / 2 calls / 0 failures. Two independent long-flow pairs reduced C to
49/44 and 57/52 rounds/calls (A 50/45 and 47/38) while median C input, selected
tokens, and resident bytes remained below A. Do not close this item yet: C
hidden success was 3/4 then 4/4 versus A 4/4 twice. The failed assertion was a
successful `RELEASE.md` edit that used `Version 2` instead of the requested
literal `v2`; it was not an edit/runtime failure, but success-neutrality is an
outcome gate, not a causal excuse. A later complete pair passed 4/4 in both
arms but regressed C to 82 rounds / 76 calls with one 30-round edit-repair
turn, versus A's 47 / 36. Task completions remained zero, so task continuity
fixed the identified lifecycle loop but is not sufficient for convergence.
Require broader clean paired repeats with no success or max-turn regression.
Evidence:
`longflow-pinai-luna-task-continuity-2026-08-24/REPORT.md` and
`longflow-pinai-luna-task-continuity-r2-2026-08-24/REPORT.md` and
`longflow-post-continuity-r3-2026-08-24/REPORT.md` and
`longflow-post-edit-anchor-r4-2026-08-24/REPORT.md`. The first post-hardening
pair passed 4/4 in both arms and restored C to 53 rounds / 51 calls / max-turn
7 versus A's 54 / 44 / 13, with no C failed outputs. This is one positive
dirty-tree pair; keep the item open for an independent repeat.

The first one-directive `retry_policy_dev` pilot exposed the opposite edge:
all four canonical cells ended without `TaskCompleted` and made no direct
`task.complete` call. Do not undo the continuity fix by making that tool
permanent. The body-free `CompletionOpportunity` candidate has landed
default-off and its deterministic already-satisfied replay is green. Two live
off/on attempts did not promote it; one proves the mechanism can execute an
offer -> lease -> explicit `task.complete` -> closure chain, but sparse arming,
worse paired efficiency and the reopened evaluator defects make the attempts
non-decision-grade.

The 2026-08-28 decision-grade rerun (post LT-RUN-05 WP1–WP5) armed once in
four on-cells and the leased decision did not call; the paired off baseline
closed a normal cell by itself. Outcomes regressed and closure did not
improve, so the frozen promotion rule ENDED the candidate — default-off is
final unless a new, separately documented design reopens it. The offer-debt
and verification-basis prerequisites both landed before the run, so the
failure is substantive (the affordance did not pay for itself), not
mechanical. See `evidence/opportunity-gate/REPORT.md` (2026-08-28 section).

Historical prerequisite text: an old mutation could combine with a newer
verifier and a crash could re-offer a key written after the last safe-point
capture; both are fixed (independent verification basis; `OpportunityOffered`
checkpoint debt with a serialization round-trip survival proof).

Surface decision (2026-08-28, rev v5): `task.complete` joined the always-loaded
production surface while `task.manage` stayed catalog-cold. The completion
acceptance gate structurally refuses premature or unverified proposals with a
typed per-turn warning. The invalid M15 v2 attempts observed many behaviorally
correct workspaces without closure, but they do not prove that discovery was
the sole cause or that v5 caused the later closure delta. Treat v5 as the
current product surface, not as an M15-promoted result; any future surface
comparison needs its own frozen paired gate.

### TOOL-PROC-01 — explicit ProgramResolver for process.run (fixed 2026-08-23)

Reproduction on Windows confirmed a tool-side semantic defect, not model
guessing: with `Command::new(argv0)` + `current_dir(cwd)`, a binary that
exists in the child cwd still failed to spawn under bare-name, `.\` and
`./` forms (CreateProcess does not search the child's cwd) while the
typed failure listing showed the same binary present — manufacturing the
exact contradiction that drives `foo` / `./foo` / `.\foo` guessing
loops. Landed: a host-owned resolver defines resolution explicitly —
absolute paths as-is; separator-relative forms join the call cwd (`..`
traversal rejected); bare names search the cwd first, then effective
PATH, PATHEXT-completed on Windows — and spawn always uses the resolved
absolute path. preflight, RetryDomain fingerprints and spawn share one
semantics; failures report the bounded candidate list they tried.

2026-08-30 Windows regression: `program_path` now preserves the native
spelling of an absolute `ComSpec` path rather than rewriting `\` to `/`; Unix
alone keeps the backslash-as-separator compatibility alias. The closure fixture
also follows `process.run`'s verbatim argv contract with one command string:
`[ComSpec, "/D", "/C", "exit /b 0"]`. Direct absolute-ComSpec execution and
the M12 deterministic closure gate cover this without reopening the fixed
resolver design.

### TOOL-DIR-01 — transactional directory creation (fixed deterministically 2026-08-28)

The pinned-serving `retry_policy_dev` preflight exposed a general ACI gap.
`fs.write` correctly refused `tests/retry_policy.rs` because `tests/` did not
exist. The passing trace then required three further model decisions and the
sequence `capability.manage(shell.exec) -> shell.exec(New-Item) -> fs.write`.
An earlier trace also tried the PowerShell builtin through direct-argv
`process.run` first. This is execution/surface cost, not Context selection;
the passing cell accumulated 119,912 TurnFrame tokens versus 8,146 historical
Context tokens.

Do not make `fs.write` recursively create parents. Its current guarantee — a
file mutation may create the final file only inside an existing directory —
keeps directory topology inside an explicit approved and recoverable effect.
Landed semantics are deliberately narrower and safer than the original
multi-component sketch:

1. `fs.mkdir { path }` creates exactly one absent final directory component;
   its immediate parent must already exist. A multi-level path is an explicit
   sequence of effects, so a single approval never hides a partially-created
   `mkdir -p` chain;
2. the host binds the exact path to the existing `WorkspaceWrite` intent with
   zero content bytes. The workspace appends an authority-v3 `Prepared`
   record before creation, then creates relative to a pinned parent handle and
   commits the stable directory identity (Unix device/inode; Windows volume/
   file index);
3. rollback removes only the exact pinned, still-empty directory created by
   the transaction. Substitution, unexpected population, cleanup uncertainty,
   or a crash after create but before the committed identity is
   `Unknown`/`Ambiguous`, never false `NotApplied`;
4. an already-existing directory is an idempotent successful value with an
   explicit no-mutation fact. File collisions, missing parents, escaped roots,
   state-directory access and link/reparse traversal fail closed; and
5. `fs.write` keeps its existing-parent boundary but its typed missing-parent
   result now derives the first creatable component after the nearest existing
   parent and names the exact `fs.mkdir` call, instead of forcing a doomed
   deeper mkdir or a shell/process guess.

The authority reader remains byte-compatible with v1/v2 file frames. New
workspace, tool-runtime, host-policy and conformance tests cover durable
reopen, rollback, precondition races, the post-create crash seam, idempotence,
zero-byte containment and confinement. The tool is catalog-discoverable but
not yet on the default surface; deterministic completion does not by itself
prove a round/call improvement.

### TOOL-DIR-SURFACE-01 — choose directory-tool admission from paired evidence

Freeze the effect semantics above. Compare the current catalog-cold baseline
against one general recovery source that surfaces the exact host-owned tool
after a trusted `ResourcePath/path_not_found` result whose recovery contract
requires topology mutation. Reuse the existing bounded surface-source/
obligation machinery; do not parse free-form model text, create a permanent
task pin, or special-case an evaluator fixture. An always-ready compact schema
is a fallback candidate only if the recovery source still costs more decisions
than its per-round schema cost.

The deterministic gate is implemented and green (2026-08-28): after a failing
mutating result whose typed metadata names the first creatable directory, the
runtime derives a turn-scoped recovery request and surfaces exactly
`fs.mkdir` with `RecoverySurface` provenance for one decision. It proves
exact-tool provenance (report row origin, unit- and actor-verified), one-
decision source lifetime (consumed by the decision that saw it, never re-arms),
unload after consumption/directive end (the requirement dies with the turn;
lease reconciliation still releases it at the directive boundary), approval
unchanged (PreferSurface demand only; a read-only gate still refuses the
recovery-marked workspace write without dispatch), and no surface change for
unrelated missing reads (observation `path_not_found` carries no
`next_directory` and never arms). Covered by
`agent-runtime` unit tests (`recovery_surface_tests`,
`surface::tests::recovery_mark_*`) and actor tests
(`tests/turn/recovery_surface.rs`). The candidate ships behind a host switch
(`with_recovery_surface`, default off): the shipped product keeps the
catalog-cold baseline until the paired gate promotes it, so the two gate arms
differ only by that switch.

The live gate is `agent-eval --recovery-surface-gate [normal|resume]` (default
two repeats): the three representative packs (create-file retry, diagnosis,
multi-file migration) run normal/resume cells with the recovery-surface
candidate switch as the only variable; every cell records its setting in
dimensions.json. It is an isolated normal/resume paired comparison on
representative create-file, diagnosis and multi-file tasks (at least two
repeats per mode — full run is 3 packs × 2 modes × 2 repeats × 2 arms = 24
cells). Promote only with equal mandatory success, lower median
aggregate rounds and calls, no new max/p95 tail, and a reported
schema/prompt-token delta. Failed outputs remain counted. After the surface
choice, rerun one bounded source-bound product preflight before formal M15;
the earlier preflight did not contain this catalog entry.

The full 24-cell paired run completed on 2026-08-28 (clean tree at
`1a239479`, serving `gpt-5.6-luna` @ PinAI `/v1`, 128k, zero NOT_RUN), but a
post-run event audit changes what it can decide. Across all 24 event streams
there are zero `RecoverySurface` rows and zero `next_directory` recovery
facts. All eight `retry_policy_dev` cells instead catalog-loaded and called
`fs.mkdir` once successfully. The treatment was therefore never exercised:
the observed off/on differences are stochastic execution/order differences,
not attributable recovery-surface cost. The report also mixes final verdict
totals (off 8/12, on 7/12 because diagnosis is 0/8) with a 12/12 versus 11/12
success statement, and its table supports higher rounds in 3/6 pairs and
higher calls in 4/6, not 5/6 for both.

Decision: **NOT_EXERCISED / no promotion**. Keep the conservative
catalog-cold baseline and the `with_recovery_surface` switch off, but do not
claim the candidate caused the 55-round tail and do not advance the
always-ready fallback. Before another live comparison, report generation must
mechanically reconstruct event-derived counts and require non-zero candidate
exposure; an exposure-free run is inconclusive rather than a rejection.
Evidence remains immutable at
`crates/agent-eval/evidence/recovery-surface-gate/REPORT.md`; this audit
supersedes only its causal interpretation.

The run also exposed an evaluator-fixture defect independent of the surface.
`retry_diag_dev` fails 0/8 because the checked-in minimal/golden solution uses
`base << (attempt-1).min(63)`, which can wrap to zero, and the deterministic
self-check does not execute `m15_diag_oracle`. This is not evidence that the
serving missed a valid golden solution. Repair the golden implementation with
overflow-safe saturation, execute `cargo test --test m15_diag_oracle` in the
fixture self-check, then regenerate the pack digest before formal M15.

Calibrated 2026-08-29 (fixture authoring, allowed before source pin; it does
not change task or oracle meaning): the diag reference solution now widens to
`u128` before the shift so large attempts saturate at `max_delay_ms` instead
of wrapping to zero, the directive and `DIAGNOSIS` text name the large-attempt
saturation requirement, and the hidden check requires a `u128`/`leading_zeros`
overflow-safe marker rather than accepting the overflow-prone shift alone.
Fixture self-check now runs each pack's oracle against the untouched seed
(reject) and the scripted solution (accept) offline, and records both pack
digests as frozen constants. The calibrated diag digest is
`2fff51573097fe4c833215420dd0da74f11a645ef5c859bdd9bba87e5b427eeb`
(was `844793249406be591372f7ee8b17bd68b3933e9d2745988168de64834584aaf3`);
the migrate digest is unchanged at
`26d69fa1d4ccd00452b3ceb88f2a6ec7fbb977989df6d6f4e2f1e345660679cb`.

A 2-cell live smoke (`agent-eval --diag-smoke`,
[`evidence/diag-smoke/REPORT.md`](../crates/agent-eval/evidence/diag-smoke/REPORT.md),
PinAI `/v1` + `gpt-5.6-luna` + Responses + 128k) ran the same day: both cells
failed on the calibration edge with now-valid evidence. The model correctly
fixed the off-by-one and named `next_delay` in `DIAGNOSIS.md`, then wrote
`checked_shl(shift).unwrap_or(max)` — which only guards shift-amount ≥ 64, not
bits shifting out of the value (`100u64.checked_shl(62)` is `Some(0)`), so the
oracle's `next_delay(63, cfg(100,1_000)) == 1_000` still gets 0. Under the old
check table that fix would have passed every needle while failing the oracle,
reproducing the audit's complaint; the calibrated needle and oracle reject it
consistently. Keep the fixture as the M15 diag pack: a failing diag cell is an
honest reported fact, not a harness artifact. The
surviving blocker is a missing task-aware completion decision boundary; see
CONV-CLOSE-02 below.

The one-cell product preflight cleared 2026-08-29 on the observation-foundation
source: `retry_diag_dev` normal PASSed on the same pinned
serving at clean HEAD `09cce69` ([`evidence/diag-smoke/REPORT.md`](../crates/agent-eval/evidence/diag-smoke/REPORT.md))
in 14 rounds / 22 calls / 1 failed output / 139,886 ms with the hidden oracle
green, 6 durable checkpoints and settlement exposed (`seen`, pre 9/15 →
post 5/7) under ordinary-final closure — no `task.complete`, no auto-close.
The one unmatched diagnosis marker is the `backoff.rs` overflow-safe needle:
the written `exponent >= u64::BITS` + `checked_mul` + saturation shape passes
the oracle but not the reference `u128`/`leading_zeros` needle text, so it is
a needle-shape miss, not a functional failure. The calibrated fixture is
solvable on the pinned serving; the earlier smoke failures were the model not
solving the overflow edge. The resume arm passed the same one-cell preflight
same-day at clean HEAD `65f6cc8` (two resumed turns, 5 + 4 rounds / 19 calls /
0 failed outputs / 104,516 ms, hidden oracle green, settlement exposed
pre 8/19 → post 1/0, ordinary-final closure, the same single needle-shape
miss). Both one-cell preflight arms cleared, but the three subsequent formal
windows all failed. M15 stays open and no fourth unchanged retry is allowed.

### CONV-CLOSE-01 — Completion Convergence observation foundation (landed 2026-08-29; causal interpretation superseded)

The 55-round / 129-call `retry_policy_dev` resume cell is the current bounded
readiness blocker, but `task.complete` itself is not established as its root
cause. The schema was present on every round. Across the 24-cell run it was
called 18 times and every tool result was successful; 17 calls reached a
`TaskCompleted` event, while one successful proposal did not reach a
`TaskCompleted` event within its trace. The long-tail cell made no
`task.complete` call and still performed formatting, linting and artifact
cleanup in its last rounds. Five other no-call cells ended their turn normally.

The stronger hypothesis is a missing completion decision boundary after the
last authoritative mutation and current verification, amplified by fragmented
verification and workspace noise (`target/` and `Cargo.lock` are model-visible
through `git.status` but evaluator-ignored). Implement the next task in this
order:

1. Make fixture cleanliness and allowed-diff visibility agree; generated build
   artifacts must not create model-visible cleanup work that the evaluator
   silently discards.
2. Add event-derived convergence metrics before policy: last authoritative
   mutation, current verification basis, first settled candidate, terminal
   mechanism, rounds/calls after settlement, outcome-free actions, and repeated
   read/diff/verify or artifact-cleanup actions.
3. Reuse the bounded `TaskRecord.resume: ExecutionState` and verification
   basis to derive dynamic states `Working -> VerificationDue ->
   VerifiedCurrent -> SettledCandidate`. No fixed round count establishes a
   state. A new mutation, obligation, failed/stale verification or unresolved
   constraint returns the task to `Working`.
4. At `SettledCandidate`, preserve model choice among an ordinary final answer,
   `task.complete` for whole durable-task closure, or one concrete remaining
   blocker/action. Runtime must not auto-close the task, suppress legitimate
   exploration or turn a subtask boundary into whole-task completion.
5. Prove deterministic scenarios first: ordinary final, durable closure,
   genuine remaining work, mutation after verification, stale verification,
   proposal settlement across cancel/resume, and cold resume. Run a small live
   gate with at least two paired repeats only after those scenarios and exposure
   accounting are green.

Landed 2026-08-29 as an observation foundation:

- Slice 1 cleanliness: `ensure_workspace_git` now writes `.gitignore`
  containing `.focus-agent/`, `.gate/`, `target/` and `Cargo.lock`,
  matching the evaluator's allowed-diff skip policy, so build artifacts are
  no longer model-visible-but-evaluator-ignored cleanup work. Remaining
  slice-1 sub-items (mechanical event-derived report reconstruction and
  treatment-exposure accounting) stay open with the live gate.
- Slice 2 metrics: `RunMetrics` aggregates the first settled-candidate
  frontier event and reports `settlement: seen / pre_rounds / pre_calls /
  post_rounds / post_calls`; event-derived and unit-tested.
- Slices 3–4 execution-local state observation: `SettlementLabel`
  (`Working | VerificationDue | VerifiedCurrent | SettledCandidate`) is
  derived by `ExecutionState::settlement()` from verification validity plus
  the typed obligation ledger — never from fixed round counts; any new
  mutation, obligation or stale/failed verification returns the execution
  state to `Working`. The label is published on `ExecutionFrontier` only on
  change, and `TaskProgressView.settlement` is populated. The stale runtime
  comment describing
  `task.complete` as catalog-cold was removed (the v5 registry always loads
  it).
- Slice 5 deterministic proof: seven actor-level scripted scenarios over the
  real runtime are green — ordinary final, durable closure, genuine
  remaining work, mutation after verification (reopen and re-settle), stale
  verification (no re-settlement without a fresh verify), proposal
  settlement across suspend/resume (durable closure commits with a Current
  verification), and cold same-run restore (reopen and re-settle).
- Remaining slice-1 exposure accounting landed 2026-08-29 with the gate
  runner: the cell summary now carries event-derived settlement facts
  (`settlement_seen` plus pre/post rounds and calls), cell outcome lines
  render them, and the new `--conv-gate` runner (`retry_policy_dev`,
  normal/resume, at least two repeats) marks any cell with zero
  settlement exposure as inconclusive rather than a pass.
- The live observation run completed 2026-08-29 on the pinned serving
  (`crates/agent-eval/evidence/conv-gate/REPORT.md`): 4/4 cells PASS with
  4/4 settlement exposure and durable closure by the model's own
  `task.complete`. This proves event exposure and ordinary task success only;
  it does not prove a model treatment effect.
- Read-only `--conv-tail` then sliced the post-settlement tail at the event
  level: the normal arm is clean (0 failed outputs, ≤4 `no_progress`
  deltas after the settled label); the resume median is driven by resume
  r1 alone, whose "long tail" is real phase-two development after an early
  seq-89 settled label (19 `advanced`, 4 Known mutations + 11 Unknown
  invalidations, 15 `no_progress`, 8 failed outputs) — each mutation
  returns the derived state to `Working` and the fresh verification
  re-settles it, exactly the designed behavior, and resume r2 is clean.
  This characterization is diagnostic only because the counter never closes
  the first candidate episode when later work reopens.

Post-review correction: the claimed decision boundary was not model-visible.
`TaskProgressView.settlement` is not passed through
`PromptAssembler::render_task_progress`, and `TaskProgressView::is_empty` also
does not make that field sufficient to emit a progress block. The current
eligibility function sees verification validity plus the execution obligation
ledger, but not `TaskAnchor` acceptance criteria/open loops/next action, known
failed commands, in-flight cleanup, or the current user/task epoch. Finally,
`--conv-gate` has only normal/resume cells with both candidate switches off;
there is no treatment/control arm. Normal versus resume is not an off/on pair.
The report remains immutable, but its causal and efficiency interpretation is
superseded. CONV-CLOSE-02 owns the correction.

The bounded progress-payload contract remains: it may retain only the current
goal, unresolved constraints, checked file identities/revisions (not file bodies), latest
verification basis/result, deduplicated known failed commands and one next
action. Every collection is capped and superseded by stable identity. It is a
resume/control summary, never an append-only transcript. The stale runtime
comment that described `task.complete` as catalog-cold was removed; the v5
registry is the source of truth and always loads it.

This slice does not revive the failed `CompletionOpportunity`, add standing
prompt pressure, expand the transcript, retune Context/GC, introduce a
TaskGraph/learned planner, or specialize behavior for a fixture/provider. Its
corrected promotion gate is owned by CONV-CLOSE-02 and uses task-aware episodes,
not the first-candidate lifetime tail.

### CONV-CLOSE-02 — historical implementation record (superseded by the 2026-08-30 merged queue)

This section records what landed and what the original checker reported. It is
not the current action plan. In particular, the report's mechanical FAIL is
preserved, while its settlement-effectiveness interpretation is now
INVALID/CONFOUNDED. `COMPLETE-AUTH-01`, `CONTINUATION-EPOCH-01`,
`ACCEPT-RECEIPT-01`, `FAILURE-LIFECYCLE-01`, `EVAL-CAUSAL-01` and
`EVAL-EPISODE-PAIR-01` own the correction.

Root cause: the first slice answered “is the current execution world verified?”
but named the strongest answer `SettledCandidate`, which sounds like “is the
task done?”. Those are different predicates. The next implementation must keep
the cheap execution-local predicate, then add an actor-owned task eligibility
join over bounded existing authority. It must not move task state into Core or
Context and must not add transcript history.

The target algorithm is a monotonic-within-epoch, invalidation-driven join:

```text
execution_ready = trusted verification is Current on
                  (task verification revision, directive revision,
                   workspace revision)
                  AND no in-flight/cancel cleanup
                  AND no open execution obligation
                  AND no unresolved failed command

task_ready      = execution_ready
                  AND current user/task epoch matches
                  AND TaskAnchor.open_loops is empty
                  AND TaskAnchor.next_action is empty
                  AND every bounded acceptance criterion has current,
                      explicit evidence coverage

label           = SettledCandidate only when task_ready
                  else VerifiedCurrent when execution_ready
                  else VerificationDue or Working
```

Acceptance coverage must be bounded, criterion-addressed and evidence-linked;
free-form “done” text is not proof. Reuse the current anchor CAS, verification
basis and evidence refs where they can express the fact; add only the smallest
typed coverage primitive if they cannot. With no declared coverage,
fail closed at `VerifiedCurrent`. A new directive/constraint, anchor-boundary
revision, accepted mutation, failed command, stale/failed verification, opened
loop or non-empty next action invalidates task readiness immediately. Progress
updates may clear readiness only through the existing anchor CAS; they never
rewrite user constraints.

Delivery order and exit:

1. Correct derivation and add deterministic negatives for unmet acceptance,
   open loop, next action, failed command, new directive, boundary change,
   in-flight cleanup, mutation-after-verify and cold restore. Preserve the
   existing ordinary-final and durable-completion positives.
2. Keep model projection absent/default-off until step 1 is green. Then wire one
   neutral settlement fact through `PromptAssembler`; add request-level tests
   proving it is present only for a task-aware candidate and that the whole
   TASK PROGRESS block stays within 2,048 characters. No stop instruction,
   auto-close or tool-surface lease.
3. Replace first-candidate lifetime counters with settlement episodes. An
   episode starts on entry to `SettledCandidate` and ends on the first reopening
   transition or terminal outcome. Report episode rounds/calls/failures plus
   whole-cell totals; do not charge legitimate phase-two work to an earlier
   episode.
4. Build a true off/on runner. Both arms use the same source, pack, serving,
   mode and repeat; only the model projection switch differs. Run normal and
   resume in both arms with at least two paired repeats. Zero on-arm exposure is
   inconclusive. Promotion requires mandatory behavior/diff/resume parity, no
   lost unfinished work, lower candidate-episode rounds and calls, and no new
   maximum episode or whole-cell tail.
5. **Historical handoff (superseded):** this slice required a promoted,
   default-on projection before exact-source M15 preflight. The current route
   permits a corrected settlement-off base candidate; only an enabled
   projection requires its own causal gate. Neither route triggers prompt
   tuning or Context/GC retuning.

Progress (2026-08-29):

- Steps 1–3 landed as a bounded batch: task-aware settle (`task_ready` =
  `execution_ready` + current epoch + empty open loops / next action +
  acceptance coverage, fail-closed at `VerifiedCurrent` with no declared
  coverage); the neutral settlement fact wired through `PromptAssembler`
  behind the default-off `project_progress` switch with request-level tests
  (present only for a task-aware candidate; TASK PROGRESS stays within its
  2,048-character bound); settlement-episode counters replacing the
  first-candidate lifetime tail. Deterministic positives and negatives
  cover unmet acceptance, open loop, next action, failed command, new
  directive, boundary change, in-flight cleanup, mutation-after-verify and
  cold restore.
- Two live-cell prerequisites landed: trusted verification PASS clears
  identity-exact `failed_commands` bound to the current basis/directive/
  workspace tuple (a fresh failure re-records and re-blocks), and provider
  retry spans a request-level window (~62 s) plus a whole-cell rerun wrapper
  (up to 3 runs, 30 s/60 s backoff) so transport outages cannot silently
  censor gate evidence.
- Live acceptance data source landed: the harness patches the bounded
  acceptance declaration onto the task; the runtime binds the current
  trusted verification pass as the coverage claim for every declared
  criterion at observation time, so cells reach a task-aware candidate
  through the real boundary. `task.manage` remains the only other writer;
  public commands keep the same contract.
- Step 4 ran 2026-08-29 (approved 8-cell budget, `--allow-dirty` on a dirty
  tree; `project_progress` is the only arm difference). 8/8 cells PASS
  behavior/diff/closure/continuation, provider healthy, 0 NOT_RUN, one
  request-level retry absorbed. Verdict FAIL: pair-0 (normal r1) settlement
  exposure asymmetry (off none / on seen — the off cell recorded no trusted
  verification pass: all four `verify.run` calls used the TaskScoped
  `rust.workspace` runner, which executes but carries no exact identity, so
  the task-aware join never armed and the zero-exposure cell is
  inconclusive by rule; every exposed cell used the host-registered
  `jobrunner.exact` recipe, whose pass arms the candidate synchronously
  with its observation), marker-violation counts differ in 3/4 pairs
  (needle-shape misses the behavioral oracle tolerates, in both arms), and
  episode rounds/calls medians are 1→1 (not strictly lower). Projection
  rendering was real and arm-separated: off 0 tokens every round, on
  430–512 tokens in 14–33 of the rounds once a candidate existed. Facts:
  [`evidence/conv-gate/REPORT.md`](../crates/agent-eval/evidence/conv-gate/REPORT.md).
  Per the frozen rule the projection stays default-off and the gate returns
  to observation; no prompt tuning or Context/GC retuning is triggered.

Non-goals: changing `task.complete`, RecoverySurface, provider cache policy,
fixture/oracle wording, Context selection/GC/retrieval/prompt packing, TaskGraph,
learned planning, or provider-specific instructions. The retained C Context
advantage remains banked; this task reduces execution amplification without
spending it.

### Fingerprint v2 — preview ≠ identity (fixed 2026-08-23)

The old `resolution_fingerprint` hashed only the 20-name cwd preview and
serialized `env` as an unordered map. Landed: scope_key =
digest(cwd identity + effective PATH + resolver rules version) is stable
across epochs; fingerprint additionally digests the full bounded
directory state (all entries sorted, 4096-entry/128 KiB caps, truncation
flag hashed) plus canonically sorted env pairs. Beyond-preview changes
move the epoch; HashMap iteration order cannot.

### TOOL-EDIT-02 — canonical edit first-attempt success (confirmation met 2026-08-27; residual reliability work only)

The `v4` contract (byte-equivalent hunk decompositions accepted; byte/
revision/settlement truth preserved) reached its archival 4x3 confirmation
window: strict 12/12, gate 12/12, zero post-edit confirmation reads, with the
regression test locking the gate. No current consumer requires a golden
decomposition; reversing that choice needs a documented consumer first. What
remains is bounded reliability breadth (staged-byte accounting, external
races), not an open acceptance gate.

Do not reopen `TOOL-EDIT-01`: revision-aware exact refusals and bounded
candidates remain landed. The open gate is product reliability of the one
canonical mutation path, `edit.patch`.

Confirmed evidence: the 2026-08-22 replay of
`context-mech-convergence` found `edit.patch` 5/5 failed and
`edit.replace` 8/21 failed; all 11 multi-line `no_exact_match` refusals
were caused by `fs.read` showing LF text from a raw CRLF seed while the
edit tools matched raw bytes. Details and the non-retroactive reading are
in the evidence `REPORT.md`.

Landed implementation (not formal acceptance): uniform LF/CRLF-aware exact
matching with target-style preservation; constant-memory occurrence scans;
pre-allocation and workspace-boundary 4 MiB caps; bounded preflight reads;
duplicate resolved-target rejection; bounded missing-path/candidate output;
and one global multi-file echo cap. `fs.read` exposes a JSON-quoted path,
raw-byte revision and EOL facts (plus a bounded mixed-EOL token map). The model
sees only canonical revision-required `files[]`; the legacy single-file shape
is parser-only compatibility and cannot ambiguously bind one revision to many
files. A successful patch reports every new revision in submitted-file order
outside the optional echo cap.

The complete post-continuity r3 trace added a general recovery defect: a large
successful multi-hunk edit echo was prefix-only and hid the final changed file
tail, while model-visible ordinal `occurrence` allowed repeated low-uniqueness
`}` repairs to land on earlier braces. `edit.patch` now exposes only unique
exact anchors with enough unchanged context; ordinal input remains parser-only
wire compatibility. Both the per-file changed-span echo and the global
multi-file bound preserve head and tail with an explicit middle-omission
marker. `tool-runtime` 154/154 and `agent-eval` 129/129 are green. A short
post-change live smoke passed in 4 rounds / 3 calls / 0 failures with the first
patch committed and no confirm read or fallback. This proves model/schema
compatibility, not accepted long-flow performance; a new paired live repeat is
still required.

That paired repeat is now directional green: C 53 rounds / 51 calls / max-turn
7 versus A 54 / 44 / 13, both hidden 4/4, and no C failures or ordinal fields.
Three residual C calls read zero-byte successful verification artifacts. The
shared process output now withholds `artifact_ref` only when captured bytes are
zero and returns an explicit no-output terminal message; non-empty/truncated
captures are unchanged. The run predates that last correction, so no synthetic
call reduction is claimed. Require an independent post-output-change repeat.

That repeat validated the zero-output routing but exposed the next execution
coherence defect. Both arms passed hidden 4/4 and C made zero `artifact.read`
calls, yet C used 47 rounds / 44 calls versus A's 43 / 32. Evidence-only
results were 29 versus 16. In the largest amplified turn C already had an
exact current target body, but globally novel list/Git/catalog observations
each appeared to advance the global Evidence Frontier. Task-target relevance
now qualifies read-only frontier advancement once a directive has an exact
Fresh root: unrelated new facts stay stored and model-visible when selected,
but do not reset convergence debt. Directives without an exact root preserve
broad exploration and all warnings remain advisory. Selected exact file
bodies co-locate `workspace_identity=current`; tool schemas distinguish
`verify.run` recipe values from `capability.manage` tool names. The r5 trace
predates this correction, so require a new paired measurement. See
[`longflow-post-empty-artifact-r5-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-post-empty-artifact-r5-2026-08-24/REPORT.md).

The r6 measurement is a counterexample. Hidden success remained 4/4 in both
arms and C retained its Context advantage, but used 57 rounds / 56 calls /
max-turn 15 versus A's 49 / 38 / 7. The task-frontier advisory fired without
preventing a 15-round already-satisfied turn. Exact surface events show
decision-bound load churn: `git.diff` disappeared when the next decision
loaded `git.status`, forcing catalog reloads instead of allowing a cooperating
tool set. Runtime now keeps explicit model loads pending until exact use,
unload, or directive end, independently of one-decision called-tool result
delivery. This has deterministic cohort coverage and no Context/GC change, but
postdates r6 and is not live-accepted. See
[`longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-task-relevant-frontier-r6-2026-08-24/REPORT.md).

The r7 pair confirmed source lifetime but not convergence: the Git reload loop
disappeared and max-turn returned to 8, yet C used 62 rounds / 59 calls versus
A's 46 / 35. Eight of ten C catalog operations addressed `fs.write`,
`edit.replace`, `git.status` or `git.diff`. The isolated follow-up candidate
always surfaces only the compact universal subset `fs.write` + Git status/diff
(about 190 additional schema tokens; core about 947/4,096); `edit.replace` and
all non-core capability classes remain dynamic. See
[`longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md`](../crates/agent-eval/evidence/longflow-pending-tool-cohort-r7-2026-08-24/REPORT.md).

Three stable-core repeats support retaining that product boundary: r8 C/A was
46/46 rounds and 41/37 calls; r9 was 49/47 and 46/38, with hidden 4/4 in every
arm and one `capability.manage` call per arm. C retained 22–37% lower model
input and 61–65% lower historical Context. r9 still had a 9-round editor tail:
two sequential replacement hunks targeted the same ping-test anchor, so the
first consumed the second's match. `edit.patch` now makes operation intent
explicit (`replace` / `insert_before` / `insert_after`); insertions preserve
their unique anchor and omitted op remains parser-only replace compatibility.
The unchanged r10 live repeat passed hidden 4/4 in both arms at C/A 48/47
rounds and 41/39 calls, with identical three failed outputs and max-turn 8.
Explicit insertions were used successfully and the r9 Hello conflict did not
recur. C's remaining two patch refusals were safe ambiguous/no-exact locator
failures; neither was a filesystem settlement failure. Across r8-r10 the
median gap is +1 round / +4 calls. Retain the stable core and explicit
operations; do not admit positional or fuzzy matching from this one sample.
See [`r8`](../crates/agent-eval/evidence/longflow-stable-core-surface-r8-2026-08-24/REPORT.md)
and [`r9`](../crates/agent-eval/evidence/longflow-stable-core-surface-r9-2026-08-24/REPORT.md),
then [`r10`](../crates/agent-eval/evidence/longflow-explicit-edit-ops-r10-2026-08-24/REPORT.md).

Canonical batch path keys are acquired in sorted order before any edit
snapshot; one pinned bounded read feeds transformation, SHA-256, recovery hash
and bounded backup capture; the shared lease is retained through composite
settlement. Prepared content uses a short exclusive sibling temp, is checked
by open-handle/name identity, length and SHA before replacement, and the
installed target is checked before and after durable journal acknowledgement.
Unix mode bits or the Windows readonly bit are retained. Cleanup or rollback
journal uncertainty becomes `Unknown`; an already-landed replacement is never
reported as `NotApplied`. Mixed-EOL anchors use strict logical newline tokens
(`LF == CRLF` only), map an authorized match back to its raw UTF-8 span,
preserve physical EOLs by ordinal, and keep lone CR/non-EOL bytes literal.
Matching remains non-fuzzy and multi-file commit remains sequential with
honest partial recovery. `Effect::rollback` now returns `AgentResult<()>` as a
settlement claim: Workspace propagates cleanup/review/authority-terminal
failure; staged and composite rollback attempts every child in reverse order
and aggregates bounded diagnostics; Core installs its recovery fence and
Runtime emits `not_applied_cleanup_recovery_required` for commit-rejection
cleanup uncertainty and `execution_cleanup_recovery_required` for
preparation/execution cleanup uncertainty rather than treating either as an
ordinary rejection. Both projections discard proposed revisions/files and
retain only bounded attempted paths and diagnostics.

For Core-managed writes, authority journal v2 now lands its synced `Prepared`
intent before the deterministic `.fa-{tx_id}.tmp` is created. It records
before/after byte lengths and SHA-256 revisions. Reopen reconciliation limits
aggregate target/stage reads, refuses any file over the 4 MiB mutation bound,
and removes a staged entry only through a confined open handle after proving
regular-file type, full expected content, and name identity both before and
after hashing. Crash seams after intent persistence, stage sync, and review
record all have deterministic recovery tests; create collisions and partial or
substituted stages have fail-closed tests. Existing v1 records remain readable
with their legacy FNV-1a-64 evidence, under the same new byte bounds.
File mutations also refuse a missing parent before opening a transaction: they
may create the final file in an existing directory but never leave implicit
directory topology outside the approved/recoverable effect. Directory creation
will need its own effect contract if added later.

Post-fix evidence now exists. The source-bound, dirty-tree 2026-08-22 r4 run
used `agent-eval.tool-edit.v2` with the v3 gate over 4 fixtures × 3 repeats and
binds the implementation after rollback/recovery and filesystem P1 hardening:
12/12 raw-byte verification and 12/12 flow gate; 9/9 non-conflict first patch;
3/3 proactive stale routes; zero patch failures, forbidden fallback,
post-success confirmation reads, provenance/target/exact-hunk violations,
recovery-required or unknown settlements; 42 rounds. Total wall time was
164,417 ms and reported provider tokens were 258,325. It preserved all r3
correctness/call-quality results; wall p50/p95 were lower while token measures
were effectively unchanged. The gate independently binds calls to the frozen
task/pack/schema/source/model identities, the latest successful same-path
read, exact local-hunk fingerprints, raw final hashes, complete runtime
barriers, and the model-invisible stale-mutation boundary. See
`crates/agent-eval/evidence/tool-surface-edit-v2-diagnostic-2026-08-22-r4/REPORT.md`.

Confirmed residuals:

- the path lease is an in-process guarantee shared by clones of one
  `Workspace`, while a second official `Workspace::open` on the same root is
  refused by the authority-journal lock. Direct or authority-bypassing
  filesystem writers remain outside it, and hash→rename is not an atomic
  filesystem CAS against them;
- Unix still has narrow name/inode-check→rename and final-check→return windows;
  Windows preservation covers the readonly bit, not ACLs, alternate streams,
  hidden/system attributes or timestamps, and its parent-directory sync is a
  no-op rather than a proved power-loss barrier;
- `.focus-agent/changes.jsonl` is a serialized, flushed review log, distinct
  from the checksummed, synced authority
  `.focus-agent/authority/workspace-effects.jsonl`. Core-managed writes are
  mapped before temp creation, but a crash after authority `Committed` and
  before the review terminal can still leave review history at `Prepared`;
- the context-free `MutationTransaction::prepare` entry is retained for
  trusted tests/maintenance and is explicitly not crash-recoverable. A
  partially written, substituted, or colliding deterministic stage is not
  deleted automatically: reconciliation returns `Ambiguous` for manual
  recovery. Legacy v1 records still use FNV-1a-64, though their reads are now
  bounded; new v2 records use byte lengths plus SHA-256;
- mixed-EOL matching materializes one bounded canonical view per hunk; keep
  the simpler implementation until profiling shows it is a hot path, then a
  streaming token matcher may replace it without changing semantics;
- `fs.write` remains a blind whole-file upsert, now in the compact production
  core surface because create/replace is a universal coding operation and its
  78-token schema costs less than repeated catalog-control rounds. Execution
  is still effect/approval gated. A future compatible schema needs explicit create vs
  revision-checked replace rather than making it a second primary editor;
- the r4 live diagnostic did not exercise external-process races, process
  crash, disk-full/journal failures, or partial multi-file recovery, and does
  not yet aggregate staged bytes. Deterministic unit tests now cover three
  Core-managed prepare crash seams and conservative stage cleanup; real
  child-process fixtures cover abrupt kills at prepare, right after commit,
  and mid-batch (staged-byte frames verified intact), cross-process journal-
  lock races (refused second official writer, retry-window handoff), and
  mid-journal corruption plus checksum-valid sequence gaps. Portable
  disk-full injection has since landed behind the `test-faults` storage seam.
  A successful edit currently performs the snapshot
  plus repeated bounded full-file integrity hashes; treat that as a candidate
  measurement, not an established performance hotspot.

Before removing an integrity pass, add a test/benchmark-only counter with zero
production-path branching. For 4 KiB, 256 KiB and 4 MiB single/two/16-file
cases, report file-read bytes, SHA/FNV bytes, staged-write bytes, review and
authority journal bytes, file/directory sync counts, and replacement/changed-
span/journal amplification. The current nominal changed-file path has about
`2N + 3M` full-file handle reads for input `N` and result `M`; caching may make
physical I/O different. Fuse a pass only after measurement and only if stale,
staged-integrity, post-replace and final-ack truth remain independently
provable.

At the r4 stage, the next formal-acceptance blocker was a run of the same
frozen pack on a clean source tree; r4 deliberately used `--allow-dirty`, so
all manifests say `git_dirty=true` and `acceptance_eligible=false`.
Acceptance measures
non-conflict first-patch success, correct proactive/reactive stale recovery,
edit-to-passing verification, failure class, fallback-to-shell/`fs.write`,
confirm reads, rounds, tokens, p50/p95 latency, bytes read/staged, commit
conflicts and partial recovery. Safety refusals may be a separate class, but
remain in end-to-end task success/time/cost. Deterministic fault/race
coverage is landed through 2026-08-26, including portable disk-full
injection behind the `test-faults` storage seam (intent append, staged
bytes, committed record) with fail-closed recovery fixtures. M12 and M13
mainline does not move.

The first clean-tree frozen run (2026-08-26, PinAI/Luna) did not close the
gate and produced two findings instead. First, a real contract drift: the
unique-anchor schema made an explicit `op` required on every hunk after the
v3 gate was authored, so the model correctly followed its tool spec and the
gate rejected all 12 cells as non-canonical while strict raw-byte truth
passed 12/12. The gate now accepts exactly the runtime enum values
(`replace`/`insert_before`/`insert_after`) alongside the legacy omitted-op
spelling, with a regression test; the drifted run's bundle is archived out
of tree. Second, the post-fix rerun scored strict 11/12, gate 9/12,
non-conflict-first 7/9 over wall 1277 s (prior r4: 463 s): one cell lost
its provider session before any tool call (`usage_incomplete`, zero patch
attempts), one crlf cell spent its first attempt on revisions not from the
latest reads and recovered on the second, and one stale-recovery cell added
a post-edit confirmation read that this fixture's flow gate forbids. No
runtime regression is claimed — the residual failures are provider latency
plus model behavior variance. At that point TOOL-EDIT-02 still awaited one
stable provider window meeting 12/12 strict, 12/12 gate and 9/9 non-conflict
first patch; the v4 conclusion below supersedes that route.

A third run the same day in a normal-latency window (`tool-surface-edit-
v3-clean-tree-2026-08-26-r2/`) confirms the separation and narrows the
diagnosis: strict 12/12 again, gate 8/12, non-conflict-first 7/9, wall back
to 280 s with no session loss. Every applied patch across all 36 clean-tree
cells has been byte-perfect; the four gate failures were two post-edit
confirmation reads this fixture's flow contract forbids, one stale-revision
first attempt (recovered), and one non-exact first-hunk attempt (recovered).
No raw-byte mismatch was observed in these frozen cells; what fluctuates
between provider windows is first-attempt decision quality. This is evidence
for the bounded pack, not proof of an editor-engine property in general.

A fourth run the same day (`tool-surface-edit-v3-clean-tree-2026-08-26-r3/`),
launched immediately after a green single-cell availability smoke in a
healthy window, reproduces the pattern once more: strict 12/12, gate 9/12,
non-conflict-first 8/9, wall 570 s with no session loss. The three gate
failures are two post-edit confirmation reads forbidden by the
stale-recovery flow contract (the same fixture passed cleanly with zero
confirm reads on its third repeat) and one non-exact first-hunk attempt
recovered on the second. Across all four clean-tree runs every applied
patch has been byte-perfect; strict raw-byte truth has never failed except
the one cell whose provider session died before any tool call. The gate bar
(12/12 strict, 12/12 gate, 9/9 non-conflict-first) remains unmet by this
provider serving; at that point the item still awaited a window where the
served model's first-attempt discipline held across all twelve cells.

A fifth run the same day (`tool-surface-edit-v3-clean-tree-2026-08-26-r4/`),
in the fastest window yet (wall 218 s, no session loss), reproduced the
verdict shape exactly: strict 12/12, gate 9/12, non-conflict-first 8/9,
with the three gate failures again being two forbidden post-edit
confirmation reads on the stale-recovery fixture and one non-exact
first-hunk set recovered on the second attempt. Five independent windows
have now produced byte-perfect applied patches in every cell that reached
a tool call, with gate failures drawn from the same two model-behavior
shapes. The diagnostic has saturated: further same-day retries against
this serving added no information, so the next historical step was a materially
different provider/model serving rather than another same-window attempt.

That materially different serving ran the same day: one clean-tree gate on
the local OpenCode relay (`ox-alpha-free`, availability precondition green,
`tool-surface-edit-v3-clean-tree-2026-08-26-ox-r1/`) scored strict 11/12,
gate 6/12, non-conflict-first 8/9. The strict miss and one cell's transport
death were a relay stream decode failure after two rounds; every completed
cell applied byte-perfect patches, and the model showed perfect hunk
discipline — no non-exact first attempt and no wrong-revision selection in
any window. Its behavioral failures are narrower than Luna's but heavier:
all five are the same forbidden post-edit confirmation read (5 of 11
completed cells). Cross-model summary after six runs: every completed
frozen-pack cell was byte-correct, while first-attempt flow discipline varied
and both servings repeated the post-edit confirmation read. This separates
the observed failure classes without generalizing the finite pack into an
engine-wide proof.

Root cause of the binding violation (event-level audit of
`mixed_eol/r1` in the ox-r2 window): after a successful patch whose echo
already carried the full committed post-state and the new revision, the
model issued a second `fs.read` returning byte-identical content and then
narrated "**Result verified:**" over lines it already possessed — a
trained-in verify-after-mutate habit, not an information need. The rule
forbidding it lives only in grader config (`max_confirm_reads_after_success:
0`); no model-visible surface states it. Surface archaeology shows the
contract used to be visible: the patch tool description carried "so
chained hunks need no confirm re-read" until the v3 surface compaction
dropped it under that tool's 96-char description cap, while `edit.replace`
still carries its twin sentence today. Fix: state the contract on the
success echo itself ("patch applied and committed; this echo is final,
no re-read needed"), which costs ~15 tokens on successful patches only
and leaves the 96-char schema cap intact.

Validation on the same relay serving, clean tree at the fix commit
(`tool-surface-edit-v4-clean-tree-2026-08-26-ox-r2/` and `-ox-r3/`, both
with REPORT.md): strict 12/12 in both, and post-edit confirmation reads
are gone — 0 of 24 cells across the two archived windows, versus every
prior window on either provider producing them. Wall time dropped from
871 s to 509/594 s with rounds 46 → 42, the visible saving from the
eliminated confirmation round-trips. A first same-day window ran before
these two with flags mis-ordered (`--evidence-dir` after
`--tool-edit-run`) so no artifacts persisted; its console verdict was
strict 12/12, gate 12/12, non-conflict-first 9/9 — observed but not
archival evidence. The remaining bar-blocker is now solely the
`batch_two_file` exact-hunk cell (~1 of 3 repeats per window): the model
merges each file's two anchor lines into one multiline hunk; bytes are
always correct and `confirm=0`. The chosen product contract is committed
byte/revision/settlement truth: hunk partition is not model-visible authority
and no current consumer requires the golden decomposition. `exact_hunks` is now
versioned to accept byte-equivalent decompositions while retaining paths,
strict bytes, revisions, settlement and no-fallback/no-confirm checks; the gate
is `agent-eval.tool-surface-edit.v4`. A future canonical-granularity rule
requires a documented consumer before reversing this decision. One archival 4x3
confirmation window on the versioned gate is now landed:
`tool-surface-edit-v4-clean-tree-2026-08-26-r4` scored `strict 12/12 gate
12/12 non_conflict_first 9/9` with zero confirmation reads. TOOL-EDIT-02 now
meets its product contract on the frozen surface.

## Open P1 — runtime scheduling correctness

Design + invariants: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)
("Lifecycle clocks and maintenance scheduling"). The tool-lifecycle clock
defect (load/execute advancing the shared tick), the O(R²) repeated
tool-scope closes, and the every-round `BeforeModel` minor scan are
**fixed**; their write-ups live in that section.

### SCHED-01 — BeforeModel runs a full minor scan every round (fixed 2026-08-23)

Measured: 77 `BeforeModel` maintenances per 15-turn cell, most with no
pending state change (`gc_work_batch` 4096 ≫ heap size, so each pass
rescans Resident+Warm). Landed: the engine stamps `last_maintained_seq`
at each completed pass, so `BeforeModel` at an unchanged sequence is a
true no-op — default report, no scan, no sequence consumption;
lifecycle-closure triggers always run. Bounded dirty batches
(`MaintenanceDebt`) remain the later step before touching scan width.
CPU/lock/event work — no extra-round causality claimed.

### SCHED-02 — search candidate completeness contract (**closed 2026-09-04; contract introduced 2026-08-23**)

The shared index bounds tokens/doc (64), postings/token (4096) and body
text to its first 512 chars, while candidate hits suppress the residual
scan — deep-body keyword recall was not guaranteed end-to-end. Landed:
catalog search returns `SearchCandidates { ids, incomplete }` with
`SearchIncompleteReason::{SaturatedPosting, TruncatedIndexedText,
UnindexedQueryShape}`;
the 2026-09-04 closure replaces the global sticky truncation bit with
per-item/filter-aware state and connects it to real body verification.
Resident/Warm bodies verify in memory; eligible Stored semantic bodies use a
fixed-concurrency owner-checked read phase outside the state lock, retain only
verified ids/ownership stamps, and revalidate the current live owner before the
unchanged O(limit) top-K ranking. Semantic bodies use exact ASCII tokens;
short/CJK query shapes use fragment-aware residuals, retaining short ASCII
tokens exactly and checking CJK substrings. A full URI resolves
directly by id, and entity/label/path substring candidates remain complete. Present checksums are
enforced; legacy rows still require valid shape/id/kind, and every missing,
corrupt or I/O failure is explicit (oversize is corrupt). Async store blobs are capped at
1 MiB, at most eight reads are active, and a query refuses rather than exceeding
256 Stored reads; the
metadata plan is O(eligible Stored descriptors), not falsely described as
O(limit). Tool/File raw bodies and body-derived entity strings are Fetch-only;
only stamped path/revision identity enters search. Direct engine and
sidecar queries enforce the same 256-character text/label and 50-result bounds
as Core. Query tokenization does not discard tokens 65+ within that char bound,
and unique-prefix saturation propagates the same incomplete signal as exact
postings.
Search completeness is explicit and implemented, not implied.

### SCHED-03 — convergence failure-cluster escalation (fixed 2026-08-23)

Invented-program PathNotFound streaks survived per-call cwd listings (8
attempts across 4 spellings) because every spelling is its own
signature. Landed: alongside the MOD-PROG-01 identical-signature
counter, `ExecutionState` aggregates consecutive same-class failures
across different targets over an unchanged world; at ≥2 distinct targets
the TASK PROGRESS view carries an EXECUTION STALL line naming tool and
class (advisory only — the model still chooses). A class change, any
world progress, or an Evidence-class observation restarts the cluster;
the per-signature threshold stays at 3.

### SCHED-04 — reread motive attribution instrument (instrument landed 2026-08-23)

Latest long-flow: fs.read 21 / repeats 18 with Warm=Stored=0 — rereads
are descriptor-only (12) and needs-revalidation (7) motives, NOT Context
GC reclaims. Landed: the `protocol-checkpoint-body-missing` motive class
identifies identity-only reads of a body the model already consumed
(read-provenance fact, unchanged digest, descriptor residency), split
out of descriptor-only/needs-revalidation so a protocol body cache would
be sized against real demand. Residency loosening stays rejected on
current evidence; the tiny current-turn LRU gets built only if this
motive shows up in live runs.

### EXEC-REV-01 — verification basis diverges across consumers (fixed 2026-08-27)

Landed: `TaskAnchor.verification_revision` is an independent basis (serde
default) bumped only by authoritative boundary changes — original goal,
constraints and acceptance criteria — while progress/open-loop/next-action
maintenance advances only the whole-record CAS revision; `TaskManager` syncs
`ExecutionState.verification.spec_revision` to the basis, and facts, exact
verifier sources, freshness/validity and the opportunity key
(`opp/{task}/a{basis}/d{directive}/w{workspace}/...`) all read the basis.
Per `LONG_TASK_EVALUATION.md` Slice B, accepted criteria move the basis too
(they are the authoritative verdict) while model-derived criteria remain
proposals — `task.manage` cannot submit the field, so no criterion-level
approval gate is implied. `validity()` additionally refuses Current when the
last evidence row is bound to a different basis than the live one, so no
consumer can disagree if the basis ever moves without the `SpecChanged`
side effect. One cross-consumer regression covers progress-only movement,
an acceptance-criteria change and a checkpoint round-trip, asserting that
ActiveTurn validity, completion, exact reuse and the derived completion
opportunity agree in all three phases. Residual, tracked with the
cold-resume matrix: the persisted offer key must also accrue checkpoint debt
(it does since 2026-08-28) with a crash-window proof that once-per-basis
discipline survives recovery.

Original observation: a progress-only CAS advances the task/execution anchor revision
without marking the existing verification stale. `ExecutionState::validity()`
and completion therefore accept it as Current, while exact reuse and
`CompletionOpportunity` require the fact's old anchor revision to equal the
new record revision and reject the same PASS (`agent-runtime/src/task.rs`,
`execution/state.rs`, `opportunity.rs`).

Risk: completion, resume/reuse and advisory closure do not share one definition
of current verification. The existing test proves only the enum remains
Current; it does not prove agreement among those consumers.

Required fix and exit evidence: introduce an independent verification-basis
revision or digest while retaining the whole record revision only for CAS.
Define criterion origin/authority, bind facts to the shared basis tuple, and
use one currentness predicate in ActiveTurn, task resume, completion, exact
reuse and opportunity derivation. Cover progress-only movement, authoritative
boundary movement and checkpoint round-trip in one cross-consumer regression.

### CONV-01 / CONV-02 / PROTO-EVID-01 — closed 2026-08-23

All three landed in Execution Convergence V1 (see
[`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md) and
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)); write-ups moved to
the second-round section below and to the closed archive. The remaining,
narrower residuals are CAP-OBS-01 and CONV-03 there.

### LONGTASK-03 — acknowledged checkpoints are cold-restorable (closed 2026-08-28; historical record)

Landed toward this fix 2026-08-27: automatic safe points, instance
checkpoints and terminal completion share one assembler with the bounded
capability-generation handshake; snapshots validate before persisting;
terminal completion is two-phase (durable prospective-terminal acknowledgement
before any in-memory commit or `TaskCompleted`, failed writes leave the task
pending/retryable); store input/output bounds and bounded retention landed.
Fixed 2026-08-28: acknowledged artifacts — including the final terminal
snapshot — cold-load, validate and restore into fresh Runtime instances in
the deterministic gate (third-instance phase), with the capability plane
handshake-verified and its generation published on every acknowledgement.

Observed: completion clears `TaskManager.active`, then captures the final
checkpoint while the actor still carries `current_task_id`; restore validation
rejects that mismatched authority state. The write path can acknowledge the
artifact without first calling checkpoint validation. Automatic safe-point
capture also does not reuse the bounded capability-generation handshake used
by external instance checkpoints (`agent-runtime/src/actor/turn.rs`,
`actor/safepoint.rs`, `checkpoint.rs`, `instance.rs`).

Risk: `CheckpointDurable` can name an artifact that no fresh Runtime can
restore, or a torn cross-plane snapshot. Whole-file load, serialization and
retention are also not bounded for a genuinely long run.

Required fix and exit evidence: route all checkpoint writes through one
capture -> stable-generation merge -> validate -> persist path. Use two-phase
completion: prepare a prospective terminal snapshot with both task authorities
cleared while the live task remains pending/retryable; acknowledge it before
committing in-memory completion and emitting `TaskCompleted`. Reject oversized
header/payload/artifact input and define bounded local retention. Load and
restore every acknowledged test artifact into a fresh Runtime and Context,
including the final artifact; stress `configured_retention_limit + 2`
checkpoints and the byte ceiling without deleting the latest required, pinned
or referenced recovery artifact.

### LONGTASK-04 — resume durability uses a snapshot fence (closed 2026-08-28; historical record)

Landed toward this fix 2026-08-27: actor-owned monotonic snapshot sequences
replace the anchor-aliased watermarks (`agent-runtime/src/actor/safepoint.rs`),
acknowledgements retire exactly their artifact's debt set, continuation
requires no outstanding debt / no in-flight write / no failed write plus a
landed sequence, and the allocator watermark rides the checkpoint lineage
without moving backwards on restore. Fixed 2026-08-28: the deterministic
gate's resume phase consumes only the acknowledged artifact tuple
(artifact, checksum, sequence, capability generation) through the verified
cold-load path, with third-instance terminal restore closing the matrix.

Observed: `resume_state_revision`, required revision and durable revision all
alias the task-anchor revision. Workspace mutation and verification can change
the checkpoint without advancing it; task changes can lower it; zero also
means both "none" and a real first revision. The continuation gate compares
only numbers and does not require no debt, no failed write and no in-flight
write. A newer debt may also appear while an older write is awaited
(`agent-runtime/src/actor/safepoint.rs`).

Risk: an older durable acknowledgement can satisfy a newer state, including
after the newest write failed.

Required fix and exit evidence: add an actor-owned, monotonically increasing
snapshot sequence independent of task anchors and persist it across the
existing durable Runtime lineage. Bind each acknowledgement to lineage +
sequence + artifact + checksum, and allow continuation only when there is no
debt, failed write or in-flight write and `durable_sequence >=
required_sequence`. Cover same-anchor distinct snapshots, out-of-order ack,
new debt during an old write, revision zero, task switch and failed-write
retry.

### EVAL-05 — resume twin does not prove the latest settled cold artifact (fixed 2026-08-27)

Landed: the harness correlates the checkpoint artifact with its snapshot
sequence and checksum across the resume boundary — a sequence mismatch fails
harness-side before restore — and tracks the full durable tuple
(artifact, sequence, checksum), requiring the durable-after-mutation ack to
match the last resume-committed sequence.

Original observation: the harness now builds a fresh engine and uses the verified loader,
but treats any `CheckpointDurable` arriving after a mutation as the mutation's
checkpoint. A pre-mutation capture may acknowledge after a later write; the
cross-phase state retains only the artifact name, not the acknowledged
sequence/checksum/capability generation (`agent-eval/src/long_live.rs`).

Risk: phase two may restore an older Runtime snapshot while the workspace is
already ahead, so the current cold-resume and continuation dimensions are not
decision-grade.

Required fix and exit evidence: correlate the latest `TaskResumeCommitted`
with its exact lineage, sequence, artifact, checksum and capability generation. Cancel
only after that tuple is fully settled; carry only that tuple across phases;
reject an older or mismatched acknowledgement before restore.

### EVAL-06 — isolated oracle setup failures are misclassified as behavior failures (fixed 2026-08-27)

Landed: the harness pre-creates the tests directory, classifies setup and
injection failures as typed `not_run` instead of behavior fail, runs the
workspace self-check before oracle injection with a distinct cargo target so
the oracle cannot pollute it, and removes the injected oracle after the run.

Original observation: oracle injection writes under `tests/` without first creating the
directory. Four retained `behavior=fail` records are actually setup failures
(`os error 3`), not executed behavioral assertions. The injected oracle also
overwrites the recorded cargo argv, blurring it with the workspace self-check
(`agent-eval/src/long_live.rs`).

Risk: setup and harness failures are charged to model behavior and can support
an invalid promotion conclusion.

Required fix and exit evidence: pre-create harness-owned paths, keep oracle
and workspace-self-check commands separate, and run the self-check before
injection (or through a target proven to exclude the oracle). Remove the oracle
before any later self-check. Type setup/start failures as `not_run(reason)`.
Only an oracle that actually executes may produce behavior PASS/FAIL; add pass,
assertion-fail, missing-directory, accidental-self-check-inclusion and
spawn-fail fixtures.

## Second-round review 2026-08-23 — trust & obligation

Source: the 2026-08-23 post-convergence full-repo review
([`TRUST_AND_OBLIGATION_TODO.md`](TRUST_AND_OBLIGATION_TODO.md), items
4–31). Clean n=2 longflow evidence: all four arm-runs passed hidden
verification, but C r2 rebuilt a process-guessing chain while global
frontier metrics stayed under threshold — progress-as-global-scalar
cannot see blocker debt, and three correctness/trust holes were
confirmed.

### PROMPT-AUTH-01 — restored raw body entered System role (fixed 2026-08-23)

The protocol body cache rehydrated file bodies through the runtime Focus
frame, which renders as a System-policy message — elevating attacker-
influenceable file content above the operator's instructions. Landed:
RESTORED TURN BODIES now ride the user-role context frame; regression
test asserts restored content never reaches focus_frame/system_policy.

### EXEC-EVID-01 — resource evidence had no currentness bound (fixed 2026-08-23)

Resource-validity evidence rows stayed visible after their file changed
(the digest was checked only against a fact table that the mutation
itself had already refreshed). Restore trusted no bounds on the new
convergence fields. Landed: one `evidence_is_current` predicate shared
by projection and sweeping (WorkspaceRevision equality; Resource needs a
Fresh fact at the identical digest, evaluated after this round's facts
land; Turn requires the current turn); `validate_execution_state`
additionally rejects oversized evidence/deltas/targets/obligations and
per-row string overruns, so restore cannot trust an unbounded checkpoint.

### CAP-OBS-01 — dynamic producer metadata must not become trusted execution facts (open, narrowed)

`ToolOutput::file_path/file_revision/resource_touches/is_verification/
may_mutate_workspace` read producer-stamped metadata, and
`take_runtime_diagnosis` read producer `failure_class`/`recovery_hint`.
Operator-trusted builtins are fine; a dynamic capability could forge
`path`/`revision`/`verification`/`mutates_workspace: false` and feed
ResourceFact, Verification, WorkingSetSignal, TASK PROGRESS authority.
Landed 2026-08-23: fail-closed routing-layer sanitizer
(`sanitize_untrusted_producer_output`) strips reserved diagnosis keys
before Core reads them plus the producer-authority keys from every
capability output; contract direction written into
[`TOOL_RESULT_ENVELOPE.md`](TOOL_RESULT_ENVELOPE.md). Typed-facts
substrate landed 2026-08-26: `ToolExecutionFacts` in
`agent-contracts/src/execution_facts.rs` carries resources / mutation
bound / verification stamp / runtime diagnosis as typed values with
constructors that mirror the legacy accessors exactly and default to
empty facts; no consumer reads them yet and there is no durable wire
form. First lane landed 2026-08-26: `ToolDispatcher::execution_facts`
(default empty) lets the operator-trusted builtin registry translate its
own stamped outputs at one sanctioned point inside the trust boundary,
capability-routed results contribute empty facts by routing, and the
actor's body-free batch ledger now consumes those facts instead of
re-deriving from producer metadata. The turn frame carries the same facts
on every tool-result step (`Option<Box<_>>`, serde-defaulted so old
checkpoints restore as `None`), and the prompt's fs.read body-identity
hints read them with a legacy-metadata fallback exactly for `None` frames.
Heating landed 2026-08-26 as consumer-side adoption only — trusted
handlers still stamp metadata keys, so no model-visible output shape
changed: `ContextIngress::ToolObservation` carries `facts`
(`Option<Box<ToolExecutionFacts>>`, serde-defaulted so old service frames
restore as `None`); the actor forwards the turn-frame step facts at its
single observation-ingest site with zero extra dispatcher calls, and the
context engine reads heating/observation identity facts-first with a
per-value legacy-metadata fallback for frames without captured touches.
Still open before
Self-Iteration: move fact construction into individual trusted handlers,
and define the event-level durable wire form. Verification's
representation landed 2026-08-26: the no-attribution frontier entry now
reads its verification claim from the dispatcher-lane facts with a
per-value legacy fallback, while `observe_tool_attributed` keeps
pre-dispatch attribution as the only reusable-verifier authority —
producer metadata can no longer mint even the compat path's claim when
facts are present. Sequencing note: once trusted handlers
stop stamping
metadata keys entirely, the legacy derivation returns empty by
construction and every ingest/prompt fallback becomes removal of dead
code instead of a dual-path migration; removing the stamps changes
model-visible tool output shapes (pinned/convergence behavior could
shift), so consumer-side adoption deliberately keeps them for now.
Verification needs no behavioral change: the
production observation path already draws verifier authority from the
trusted pre-dispatch attribution channel; only its representation differs.

Handler-level direct construction landed 2026-08-26 as a dual-write slice
with no model-visible change. The authoritative builtin handlers —
`fs.read`, `fs.list`, `fs.write`, `edit.replace` (success, no-op and
refusal outcomes), `edit.patch` (applied, no-op, refusal), and
`verify.run` (which owns the verification claim over its wrapped process
result) — now stamp native `ToolExecutionFacts` at construction time
under the reserved `metadata._execution_facts` key;
`sanitize_untrusted_producer_output` strips that key together with the
other reserved keys, so a dynamic capability cannot mint facts by
carrying it. `BuiltinToolDispatcher::execution_facts` prefers native
stamps for owned tools and keeps the legacy key derivation as the
fallback for handlers that have not migrated; per-handler tests assert
native equals derivation on every outcome shape, so consumers switching
between channels see identical values. The legacy stamps stay because
removing them changes model-visible output shapes.

Producer-bound coverage completed 2026-08-26 the same day: every
remaining builtin family now stamps an explicit workspace-mutation bound
on its own outputs (`shell.exec`, `process.run`, `git.status`, `git.diff`,
`search.grep` including cancellation and no-match outcomes,
`code.symbols`/`code.diagnostics`, `artifact.read`,
`context.manage` across all ops, `task.complete`, `task.manage`, and the
`capability.manage` control surface), mirroring the temporary
builtin-name table exactly so the two channels cannot disagree. Shared
refusal helpers (`hidden_path`, missing-path) stay on the derivation:
they serve many tool names and their outputs carry no authority keys.
`process.session` now stamps `may_mutate=true` natively on every action (`start`/`poll`/`stop`) via the `_execution_facts` channel, and the legacy name-table fallback also resolves it to true — both channels agree on the conservative `Unknown` footprint shared with `shell.exec`/`process.run` (see [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) resolved `process.session` bound). With coverage this complete, retiring the name table reduces to retiring the legacy stamps once model-visible drift is acceptable (derivation then returns empty by construction and every fallback becomes dead-code removal), plus the event-level durable wire DTO. The session-bound decision is now closed.

### PROTO-EVID-02 — cache correctness + observability (fixed 2026-08-23)

Two findings: edit echo was cached as if it were the exact body (it is
a patch echo, not the file), and "remaining rereads are not cache
misses" was unverifiable because no counters existed. Landed:
`record_protocol_body` accepts fs.read bodies only (edits invalidate
their paths); assembly emits per-round `ProtocolBodyCacheStats`
{eligible, hit, miss, invalidated, oversize, restored_body_tokens} which
agent-eval aggregates into summary.json — hit rate is now independently
verifiable from any bundle.

### CONV-03 — obligation lineage + precondition epochs (landed 2026-08-23, second refinement)

Global frontier advance does not prove blocker resolution. Landed in
two steps: first the typed `ExecutionObligation` ledger with host-
trusted `resolution_fingerprint` preconditions and bounded UNRESOLVED
BLOCKER warnings; then — after the live run showed attempts never
escalating — the lineage model. `ExecutionObligation` now carries a
stable `scope_key` (ExecutableResolution = resolver-context digest;
path or target identity for the other domains), a per-epoch
`precondition` fingerprint, `epoch`, per-epoch `attempts`, and cross-
epoch `total_attempts`. Same scope + same fingerprint accumulates; same
scope + new fingerprint advances the epoch (**PreconditionChanged ≠
ObligationResolved**); resolution requires blocker-specific proof — an
ExecutableResolution obligation is cleared only by a success carrying
the *same* scope_key *and* fingerprint (a successful rustc build no
longer clears "compiled tests exe not found"), while EditTarget /
ResourcePath / ProjectMarker keep their target-specific proofs. Hard
refusal stays exactly as narrow as before: provably equivalent retries
only. The LaunchResolutionFact revision guard note remains: it is
deliberately conservative until fingerprints are recomputable
pre-dispatch without I/O.

### CONV-OBS-01 — obligation lifecycle is event-visible (fixed 2026-08-23)

Bundles could not prove whether blocker warnings fired. Landed:
`RuntimeEvent::ExecutionObligation {kind, domain, scope_digest, epoch,
attempts_in_epoch, total_attempts}` with kinds opened / attempted /
precondition_changed / resolved / dropped, emitted from the observation
pipeline; agent-eval aggregates `obligation_*`,
`avoidable_failure_calls` (failures after the first in one lineage),
`max_obligation_attempts_per_epoch`, `max_total_attempts_per_lineage`,
and per-user-turn tail metrics (`max_turn_rounds`, `p95_turn_rounds`)
so optimization targets long turns, not task-round means. Deferred
honestly: a `warning_surfaced` kind needs the render path to report
surfacing; do not fake it from attempt counts.

### CONV-04 — execution attribution + capability leases (partial; attribution/negative-fact/equivalence-class/obligation-provenance slices landed through 2026-08-26)

The retained long-flow event streams prove that the current convergence
scalar is the wrong decision signal for optional exploration. C/A produced
the same eight successful Known mutation outcomes, but C produced 48 versus
21 evidence-only results, 9 versus 0 Unknown invalidations, and an 18 versus
3 maximum result streak without an outcome advance. Its catalog-loaded
optional surface exposed 134 reported rows (118 unused in their round) and
received 18 requests; A exposed 28 (26 unused) and received two. All selected
reports were untruncated. The 36-call C-A difference is exactly the additional
27 evidence-only plus 9 Unknown results in this diagnostic.

Landed measurement foundation: `agent-eval::RunMetrics` aggregates the
body-free `outcome_frontier_*` partition and bounded
`catalog_optional_*` exposure/request join from existing events, renders them
under `--metrics`, and includes them in new bundle summaries. It counts
`TransientNoPersist` results, but does not persist their bodies or change
Runtime behavior. Source-bound facts and the causal trace are in
`crates/agent-eval/evidence/longflow-task-provenance-2026-08-24/REPORT.md`.

Landed first behavior slice (not yet a live performance claim): Runtime uses
source-driven schema leases rather than a round TTL. Exact tools called by one
model decision remain rooted through execution and the next successful
decision; reuse renews the result-delivery source, non-use releases it. A
trusted catalog-load receipt establishes a separate pending-use source until
the exact tool is called, explicitly unloaded, or the directive ends. Adjacent
loads therefore form a small turn-local cohort instead of evicting each other;
using one consumes only that member. New directives clear leftover ephemeral
leases, while explicit task requirements and typed verification/evidence roots
survive. Host/operator loads are a distinct persistent source until explicit
unload; Runtime/model load paths never become task-global pins, and restore
unions current composition sources with checkpoint residency without
promoting restored-only rows. `ExecutionBatchSettled` accounts
transient/refused/reused actions without persisting their bodies. Oversized
batches execute no member and terminalize every request as a no-dispatch
refusal. Builtin, dynamic capability and actor tests cover release, renewal,
reload, task-root retention, restore and source separation. Lease/batch event
append failures fence the actor before another model decision. The model tool
batch has a 32-call hard memory/queue bound; it is not a convergence constant.

Landed second behavior slice (still not a live performance claim):
`ToolDispatcher::execution_attribution` supplies bounded pre-dispatch purpose,
canonical resource targets and explicit verification-reuse policy. Runtime
joins targets with current task roots; dynamic capabilities fail closed,
shell/process remain Opaque, and output metadata cannot mint reusable
verification. Unrooted trusted path misses enter an eight-row,
workspace-revision-bound negative-fact table rather than the Obligation
Ledger. Equivalent reuse requires a live Workspace absence check plus a
successful `ExecutionNegativeFact::Reused` audit append; external appearance
or any admitted workspace mutation invalidates the fact. Current task roots
promote the next miss back to an obligation. Exact trusted verifier sources
are checkpointed under the task-anchor revision and PreferSurfaced when
verification is due, with semantic-role fallback if unavailable.
`negative_fact_*` eval counters, state/builtin/capability tests and an actor
test (two terminal read results, one real dispatch) cover the landed boundary.

Landed third behavior slice (still not a live performance claim):
`VerificationReuse::ExactCurrentWorld` requires a trusted SHA-256 host identity
digest for recipe/execution-profile/policy/environment inputs; raw environment
material is not stored. A successful
verification fact records exact tool, Runtime argument digest, task anchor,
user-directive revision and workspace revision. Runtime skips a later call
only if the whole tuple remains current and the `ExecutionVerificationPass`
reuse event appends; otherwise it dispatches. The no-dispatch result is
truthful (`executed=false`), remains a terminal action, and is split into
`verification_pass_recorded/reused` eval counters. New user directives and
any admitted workspace revision change force a real rerun. The state unit test
covers argument, environment, directive and workspace invalidation; the actor
test receives two terminal verification results from one dispatch.

Landed production entry point: bounded `verify.run { recipe_id }` recipes are
the only builtin process calls that can receive Verify attribution. Model argv
cannot shadow the host recipe; Core and the dispatcher are wired from one
recipe set. General project runners are `TaskScoped` and conservatively retain
Unknown mutation semantics. The first exact recipe is the generic
  manifest-free Rust test-target compile into `.focus-agent`; it binds a
  complete bounded workspace file snapshot, recipe revision, platform,
  resolved compiler and bounded complete environment. This covers transitive
  sibling modules; links/escapes, external-input directives, special files,
  overflow and pre/post identity drift downgrade to real execution. The real
runtime/tool deterministic bench now proves two requests settle from one
spawn (`Recorded=1`, `Reused=1`). Generic shell/process behavior is unchanged.

Open implementation must remain execution-only and staged:

- extend the landed exact-current completed-PASS identity with broader
  bounded coverage/obligation provenance and explicit host-declared
  equivalence classes; do not infer equivalence from commands. The API shape
  is designed 2026-08-26 in
  [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) (coverage domains with
  declaration revisions, class membership evaluated against current
  composition, fail-closed dispatch on every miss). Slice 1 landed 2026-08-26
  dormant — no shipped recipe declares a domain, so every request still
  dispatches: attribution carries bounded provenance, PASS facts store it,
  the state predicate and actor table check implement the widened reuse,
  and the reuse event plus eval metrics gained an exact/equivalent
  discriminator. Identical
  in-flight joining inside one batch is landed and proven: duplicate
  typed-verification calls settle as one spawn plus one truthful no-dispatch
  reuse (`ExecutionVerificationPass` Recorded/Reused, batch accounting
  `(2, 2, 1, 1)`), and the same joining holds for the negative-fact path;
  the sequential single-flight actor makes a second concurrent join window
  structurally impossible;
- extend the landed exact result-delivery/task/verification roots with trusted
  obligation-scoped provenance source tools;
- complete the table-driven crash/restart matrix. Normal, transient,
  recovery-refused, duplicate, oversized-batch, scope-open, admission and
  publication abort paths now settle or expose a missing terminal through the
  actor-local ledger; abrupt-loss replay evidence landed 2026-08-26:
  trace-only `analyze_batch_interruptions` in the agent-replay recovery
  report flags rounds killed between tool start and durable batch settlement
  with exact per-call counts, keeps live settle-time missing/unexpected
  terminals as a separate integrity signal, and ignores tool events outside
  any model round instead of inventing attributions;
- independently make accepted completion one-shot and terminal-safe.
  Landed 2026-08-26 as a deterministic proof matrix through the real actor,
  independent of the retained baseline (whose zero completion calls proved
  nothing): duplicate `task.complete` proposals inside one successful batch
  commit exactly one durable record (which concurrently-settling proposal
  wins the single slot is unspecified; uniqueness, no extra model round, and
  no fence are the contract), and an accepted completion stays terminal for
  its own turn while queued user input still drains into a clean follow-up
  turn with exactly one TaskCompleted, per-turn TurnCompleted events, and a
  completed-task catalog holding exactly the accepted record. The retained
  baseline still has zero completion calls, so its 49/65 versus 38/29 gap
  must not be attributed to this edge either way.

Do not lower the 18 KiB watermark globally, choose a call cutoff from this one
trace, parse arbitrary command strings to infer read-only/verification, or add
another generic "finish sooner" prompt. Before default behavior changes,
require deterministic exact/equivalent verifier reuse, stale settlement,
transient action, negative fact, cross-turn lease, discovery/reload and
already-satisfied-task tests, followed by at least two paired live repeats
with hidden success unchanged and no new p95/max-turn tail.

### PROTO-EVID-03 — Unknown suspends body reuse instead of deleting it (fixed 2026-08-23)

First live `ProtocolBodyCacheStats` accounting showed eligibility of
20–31 rows per longflow cell with hit rate exactly 0: every command
tool carries an Unknown footprint and each one physically cleared the
whole turn cache. Fix keeps correctness and reuses the existing
revalidation loop: Unknown mutations now *suspend* entries — bytes stay
in cache but are ineligible (**CachedBytesPresent ≠ BodyCurrentlyTrusted**);
BeforeModel hash revalidation restoring the same path@digest Fresh makes
the entry eligible again, a changed digest never passes the identity gate
and is left to LRU eviction. Known mutations keep physically dropping
their touched paths; counters split `invalidated` (physical) from
`suspended` (dormant). Deterministic regressions cover both branches.
Not a Context GC change: Context policy stays frozen.

### EVAL-IMMUTABLE-01 — live evidence attempts must not overwrite (fixed 2026-08-23)

A provider-503 retry overwrote good r1 artifacts; reconstruction had to
come from harness logs. Landed: `PairSink::claim` resolves the repeat
directory once per run — existing directories are never reused;
reruns land in `r{n}-attempt{k}` and failed attempts stay auditable.

## Freeze (not a defect)

### TOOL-GC-PHASE2 — surface pressure hysteresis (landed 2026-08-23)

Post-clock-fix long-flow (`longflow-post-clockfix-2026-08-23`) kept
re-loading optional builtins mid-task (13 loads, git.diff x4) and the
model even guessed `warm.<tool>` names, so the phase-2 gate was met.
`BuiltinToolDispatcher::gc` now cools only above a soft schema-bytes high
watermark, oldest-idle first, down to a low watermark (defaults
18_000/9_000; 0 restores pure idle semantics). The protocol evidence LRU
gate also fired; that cache landed 2026-08-23 (see PROTO-EVID-02).

### CTX-11 — Execution Coherence V1

**Status: Freeze Candidate** (MOD-OBS-01 / MOD-PROG-01 / turn
checkpointing landed 2026-08-21; the clean post-outage longflow pass
2026-08-23 held — Warm=Stored rereads stayed 0 and capability churn
stayed gone). Do not reimplement `ResumePoint`. A model-visible
`TaskProgress.task_changes` projection was tested and reverted 2026-08-24:
although one attribution turn shortened, its refined run amplified C to 127
rounds / 174 calls. Do not reintroduce it without the replay and paired-live
gate in `ROADMAP.md`. A generic current-workspace-authority standing prompt
also failed that gate in two repeats (C 64/79 and 72/76) and was reverted; do
not replace structured evidence with a "stop earlier" instruction.
Contract: [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md).

## Open P2 — evaluation

### EVAL-01 — M15 live evidence is not yet auditable acceptance

Live cells must remain rebuildable from versioned per-cell artifacts
(manifest, events, verify, workspace hash). Partial `agent-eval.cell.v1`
bundles exist; executable hidden build/tests for a large suite do not.
Do not close M15 from smoke, `add_test`, or the 30-task pilot.

### EVAL-02 — layer the evals; do not reuse one A/B/C

Frozen `context-bench.v1` SPEC / pack digest stay frozen. Wave-1 live
(27 cells) is historical evidence under
`crates/agent-eval/evidence/context-bench-wave1/`. **Context** live
`context-mech.v2` (A/C × 3 tasks × 2 repeats = 12 cells) is under
`crates/agent-eval/evidence/context-mech/`. `add_test` is Tool Surface
(`historical_context=0`), not Context. Do not collect 300×3. Do not
retune GC from that live or from an `add_test` cell. Do not treat
`Likely optimization target` as a modification order.

### EVAL-03 — outcome split is partial; live attempts are non-decision-grade (fixed 2026-08-28)

Observed: dimensions are serialized separately, but overall PASS currently
requires only task completion + behavioral PASS + clean diff. It does not
require absence of runtime error, successful restore/continuation, or healthy
provider/runtime. Per-phase counters are committed only on a fully successful
driver return, restored and continued are conflated, and summary arithmetic is
not consistently rebuilt from per-cell facts. The exact verification recipe
also omits part of the mutable input set. EVAL-05 and EVAL-06 further contaminate
the retained attempts.

Risk: the two 2026-08-25 CompletionOpportunity attempts are useful diagnostics,
and one contains a real mechanism chain, but their pass ratios, medians and
arming rate cannot decide promotion. The candidate remains default-off.

Required fix and exit evidence: make every dimension typed PASS/FAIL/NOT_RUN;
require all mandatory dimensions, healthy provider/runtime and no runtime error
for overall PASS; classify provider/Runtime failures from typed sources rather
than error-string substrings; finalize phase counters even on failure; separate
restored, continued and completed; snapshot the complete bounded workspace
input set; require one non-empty identical opportunity key across
Offered -> Called -> Completed; and derive reports mechanically from per-cell
facts, including tool calls, prompt/schema tokens and tails. For `n=2`, report
both observations and any
midpoint used rather than naming the upper value as the median. Only then rerun
the frozen paired gate. Design and exit gate:
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

Fixed 2026-08-28: the LT-RUN-05 WP4 evaluator truth reconstruction landed
(typed per-dimension records incl. healthy-provider and no-runtime-error
requirements, phase counters preserved on failure, restored/continued
separated, mechanically derived summaries, for-n=2 medians reported as both
observations), and the frozen paired gate then RERAN decision-grade — its
promotion verdict (FAIL; candidate ended) is recorded in
`evidence/opportunity-gate/REPORT.md` (2026-08-28 section).

## Closed archive (index only)

Full text: git history of this file.

| ID | Closed as |
| --- | --- |
| EVAL-07 | M15 v2 evidence remains forensic-only; the typed v3/v4 reconstruction, content-addressed reporter (`ea821bb`), bounded failure-monotone harness (`f57a118`) and decision-grade windows close the evidence-projection defect. Six v4 valid FAILs are banked through `_windows/1788402676712` (10/12, 0 NOT_RUN); M15 remains open. |
| 2026-08-10 repair pass | Workspace prefix, git.diff, focus/restore fences, context-service parity, journal/restore |
| CTX-01..CTX-10 | Episode, residency, fetch/search persist, store, Storage GC, GC ops, materializer, mid-turn signals, clocks, TaskAnchor |
| CTX-06..CTX-09 | GC/storage ops, materializer budget, working-set signals, lifecycle clocks |
| CORE-02..CORE-09 | Turn durability, checkpoint, output broker, System-role leak, cancel/process cleanup, TOCTOU opens, standing grants, schema budget |
| CORE-11 | HostToolPolicy registry, manifest → operator review → atomic admission/revocation, versioned snapshots and per-binding epoch fencing landed through 2026-08-26; M12 closure audit remains under CORE-01 |
| TOOL-01 | `search.grep` cancellation |
| TOOL-ENV-01, TOOL-EDIT-01, TOOL-VIEW-01, TOOL-ERROR-01 | Tool-quality preflight 2026-08-17 |
| MOD-AUTH-01 | `edit.patch files[]` multi-file authority widening → `EffectIntent::WorkspaceWriteSet` + all-paths `grant_matches` (2026-08-21; see PLATFORM_SECURITY.md) |
| MOD-AUTH-02 | Prepared effects report canonical `ActualWorkspaceWrite` (real path + real staged bytes); Core commit rejects `ActualExceedsApproved` outside the approved set (2026-08-21) |
| LONGTASK-01/02 | Catalog-cold progress CAS, actor safe-point resume install, coalesced checkpointing and same-task continuation landed deterministically (2026-08-24/25); remaining residuals are the LT-RUN-05 cold-resume matrix items under LONGTASK-03/04 (EXEC-REV-01 closed 2026-08-27) |
| CORE-12 | M13 sandbox gate: structured attestation with per-flag mechanism proofs, `required ⊆ actual` activation, native fail-closed UntrustedGenerated. Closed 2026-08-27 on the clean-tree closure-audit report (`evidence/platform-closure/m13/`) |
| Sandbox floor | `UntrustedGenerated.required` now includes `fs_read_confined` + `cpu_quota` (still fail-closed on native until provable); `process_spawn_controlled` → `process_count_quota` with a wire-compat serde alias (2026-08-21) |
| Foreground ack | `ContextConsumptionAck.foreground_item_ids` + engine counter: foreground bodies the model saw are observable (weak signal; no residency / admission change) (2026-08-21) |
| TOOL-02 | `search.grep` `path` accepts a file target (file-or-directory), removing a class of `path_not_found` tool failures (2026-08-21) |
| EVAL identity | Live evidence runs refuse a dirty workspace by default (`--allow-dirty` opt-in); the manifest records `source_tree_digest` over HEAD tree + tracked diff + untracked `crates/` sources (2026-08-21) |
| EVAL-04 | Source-identity self-pollution: a live run's own untracked evidence output made every cell after the first report `git_dirty=true` (the `context-mech-convergence` manifests record this). Identity scans now exclude `crates/agent-eval/evidence` — run outputs are not tested sources (2026-08-21) |
| CTX-12 | Not a code divergence: the parity tests had spawned a 9-day-stale `target/debug/agent-context-service.exe` (`serde(default)` hid the wire drift). The interim mtime guard/rebuild hint from 2026-08-21 is superseded: the integration target is now owned by `agent-context-service` and uses Cargo's exact `CARGO_BIN_EXE` binary, so scoped runs need no manual service build/touch. |
| PROV-01 | `provider-openai` loopback wire test failed through machine-wide proxies (Clash/V2Ray WinINET interception → gateway 502). Fixed with `OpenAiProvider::with_client` + a `no_proxy` test client (2026-08-21); production `new` keeps auto system proxy. |
| CONV-01 | Execution Evidence Frontier: ExecutionEvidence + FrontierDelta + ConvergenceState + `ExecutionFrontier` events + eval metrics; replay rebuild + conformance serde contracts (2026-08-23) |
| CONV-02 | Cross-tool convergence debt: FailureClass/FailureDomain split, RetryDomain::ExecutableResolution with host-trusted launch facts, no K-strikes decision recorded (2026-08-23) |
| PROTO-EVID-01 | Current-turn protocol body cache: ActiveTurn LRU, checkpoint+Fresh-gated rehydration, mutation invalidation; superseded by PROTO-EVID-02 correctness/observability fixes (2026-08-23) |
| PROMPT-AUTH-01 | Restored turn bodies moved from System-role focus frame to user-role context frame with regression test (2026-08-23) |
| EXEC-EVID-01 | Unified evidence currentness predicate shared by projection and sweep + restore bounds on convergence fields (2026-08-23) |
| PROTO-EVID-02 | Body cache source narrowed to fs.read exact bodies + per-round ProtocolBodyCacheStats event accounting in eval bundles (2026-08-23) |
| EVAL-IMMUTABLE-01 | Pair sink claims fresh repeat directories (`r{n}-attempt{k}`); existing evidence is never implicitly overwritten (2026-08-23) |

Do not start sourced `EpisodeOutcome`, GC retune, or a second ResumePoint
from this index.

# Modular Runtime and Tool Ecosystem TODO

Status: code-grounded discussion draft, 2026-08-10

This document records the proposed modular boundary, builtin coding-agent
interface, extension taxonomy, and evaluation plan. It is deliberately
separate from the context work queue:

- `CONTEXT_RUNTIME_TODO.md` owns TaskAnchor, Episode, Focus, recall, and GC;
- `AUDIT_TODO.md` owns confirmed correctness/security defects;
- `ROADMAP.md` owns approved milestone order;
- this document is a design proposal until its decision gates are accepted.

Status markers: `[x]` means a committed baseline, `[~]` means partial or
known to be insufficient, and `[ ]` means open.

## Decision summary

The project should be modular, but "modular" must not mean that every module
is equally trusted or may call every other module.

The intended shape is:

```text
Composition root / product host
  provider adapters, UI, storage choice, concrete context implementation
                              |
                              v
Trusted Core ----------------------------------------------- stable
  permission policy | effect commit | sandbox authority | budgets
  durable audit/checkpoint | capability authority | runtime integrity
                              |
                              v
Evolvable Runtime ------------------------------------------ one orchestrator
  RuntimeActor | TaskAnchor | Episode/Focus | prompt assembly
  context/tool materialization | capability scheduling | multi-agent policy
                              |
                 +------------+-------------+
                 |                          |
                 v                          v
Builtin ACI ------------------------  Extension plane -------- untrusted by default
  small coding primitives             process capabilities / MCP adapters
  runtime control surfaces            skills / hooks / packaged plugins
                 |                          |
                 +-----------+--------------+
                             v
                  typed result / EffectRequest
```

There remains exactly one turn/task orchestrator: `RuntimeActor`. An
extension can provide a capability, observation, workflow, or lifecycle
interceptor; it cannot create a second task state machine, mutate the
ContextEngine directly, append arbitrary prompt messages, or commit an
effect outside the Core.

The builtin tool set is part of the agent-computer interface (ACI), not just
an incidental collection of helpers. It must be designed and benchmarked as
a product surface because tool schemas, feedback format, output bounds, and
failure semantics directly change model performance.

## Trust rings and ownership

### Ring 0: Trusted Core

The Core is intentionally small, stable, and unavailable to model-authored
extensions. It owns mechanisms whose compromise invalidates every higher
layer:

- permission and standing-grant evaluation;
- effect preparation/commit/rollback and generation fences;
- sandbox/process/network authority;
- output, time, process, memory, and tool-schema budgets;
- durable events, checkpoint barriers, recovery fencing, and audit identity;
- capability identity, signature/source, activation, quarantine, and
  resource enforcement;
- runtime integrity and the non-bypassable policy envelope.

Core policy must derive authority from the operation and granted handles,
not trust a tool's self-declared risk label. An extension may request less
authority; it can never grant itself more authority.

### Ring 1: Evolvable Runtime

`agent-runtime` remains the only orchestrator and owns adaptive policy:

- the actor turn state machine and operation generations;
- TaskAnchor, Episode, Focus, open loops, and CompletionRecord transitions;
- context maintenance/materialization and prompt assembly;
- active tool-surface selection, scheduling, and lifecycle safe points;
- multi-agent delegation/coordinator policy;
- interpretation of bounded tool results into runtime/context events.

This layer may evolve and eventually self-improve, but cannot replace or
modify Core enforcement, audit, or evaluation authority.

### Ring 2: Builtin ACI

Builtin primitives are reviewed, versioned, tightly bounded, and available
without installing an ecosystem package. They provide the minimum complete
coding loop:

```text
orient -> inspect -> locate -> change -> execute -> verify -> review -> deliver
```

They still obey the same effect, budget, cancellation, confinement, and
audit contracts as external tools. "Builtin" means trusted implementation,
not permission bypass.

### Ring 3: Extension ecosystem

External capability code runs out of process or behind another enforceable
adapter boundary. It receives scoped handles and returns typed values,
artifacts, or an `EffectRequest`; it never receives raw runtime/context/core
objects.

Browser automation, GitHub hosting, issue trackers, databases, cloud,
deployment, email, design applications, and language-specific heavyweight
services belong here unless repeated evaluation proves one should become a
default builtin.

### Composition adapters are not ordinary plugins

Model providers, concrete ContextEngine implementations, UI frontends,
event stores, and product hosts are selected at composition time. They are
operator-trusted adapters, not capabilities the model can discover and load
mid-turn. The current typed `ModuleHost` plane should be documented and
named accordingly so "module" does not imply an untrusted plugin may replace
the approval policy, journal, or ContextEngine.

## Extension taxonomy

Do not force every extension into one `Tool` abstraction.

| Type | Purpose | Model-facing | Authority rule |
| --- | --- | --- | --- |
| Tool / Capability | Adds a typed action or query | Schema is loaded on demand | Scoped permissions; effects committed by Core |
| Skill | Adds procedural knowledge and a workflow over existing tools | Instructions enter context only while active | Adds no authority of its own |
| Hook | Validates, formats, observes, or gates a lifecycle event | No direct tool schema | Cannot widen policy or silently mutate protected state |
| Provider / Adapter | Connects model, MCP, remote executor, context service, or storage protocol | Not normally exposed | Operator-installed and contract-tested |
| Plugin Package | Packages/version-pins skills, tools, hooks, adapters, metadata, and tests | Package itself is not a tool | Installer verifies source, dependencies, permissions, compatibility |

A package manifest should make each contributed component explicit. Loading
a Skill must not implicitly start a process capability; installing an MCP
adapter must not implicitly inject its entire schema catalog into every
model request.

## Current code baseline

The repository is already partially aligned with this architecture.

| Area | Status | Code-grounded assessment |
| --- | --- | --- |
| Contract-first dependencies | [x] | `agent-contracts` defines `ContextEngine`, `ToolDispatcher`, capability, effect, model, and event contracts. Concrete context/tool implementations are composed outside `agent-runtime`. |
| Single orchestrator | [x] | `RuntimeActor` owns turn state; `agent-kernel` remains a stateless facade. Preserve this boundary. |
| Typed composition modules | [x] / needs naming | `agent-runtime/src/host.rs` publishes context/model/tool/approval/event/artifact services through a typed registry. This is a trusted composition plane, not the boundary for arbitrary model-authored plugins. |
| Dynamic capability catalog | [x] / lifecycle gap | `CapabilityRegistry` validates/caches declared tool schemas, prevents name shadowing, tracks activation/maturity, and merges them through one dispatcher. Lifecycle is per tool: loading one tool never surfaces its siblings (process start/stop stays owner-level). Dynamic capabilities still do not participate in builtin idle cooling/GC. |
| Bounded active tool surface | [x] | Bounded task-owned exact-tool requirements, `MustSurface`/`PreferSurface`/`KeepReady`, the runtime-owned `RoundSurfacePlan`, bounded selection/omission/block reports with per-row provenance, source revisions, and a monotonic round `surface_revision` are implemented and verified. A typed-root policy derives family roots from the TaskAnchor/focus/active-call state at BeforeModel. RuntimeCheckpoint v2 persists requirements, anchors and counters rather than a derived snapshot. |
| Builtin effect staging | [x] baseline | `fs.write` and `edit.replace` return `PreparedEffect`; the runtime generation fence decides commit/rollback. |
| Generic shell effects | [~] confirmed blocker | `shell.exec` starts a shell in the real workspace and returns `Value`; commands may mutate before any prepared-effect fence. Sandboxing, standing policy, process-tree cleanup, and effect observability must make this escape hatch safe (`CORE-06`/`CORE-08`). |
| External effect enforcement | [~] confirmed blocker | Process capabilities currently execute inside the child and return a value, so side effects can bypass the effect fence. Keep the `CORE-01` defect in `AUDIT_TODO.md` authoritative. |
| Extension sandbox | [~] confirmed blocker | The process host has framing/deadlines and limited environment/rlimits, but no complete brokered filesystem/network boundary and no reliable process-tree kill on every platform (`CORE-06`/`CORE-07`). |
| Permission model | [~] too coarse | `ToolSpec` has only `ReadOnly`, `WorkspaceWrite`, and `ProcessExecution`; capability permissions are strings and a capability can understate real behavior. Standing grants now narrow approval at the gate (`CORE-08`), but capability-declared permissions remain coarse strings. |
| Output contract | [~] partial | Builtins generally bound output and spill artifacts; the kernel-level trusted output broker caps every model-facing field and spills oversized content once (`CORE-04`), and the actor keeps a last-line guard. `ToolSpec` still cannot declare an output budget/spill policy, and process capability output/stderr needs one enforced path. |
| File/navigation tools | [~] useful baseline | `fs.list`, ranged `fs.read`, `search.grep`, `fs.write`, and exact `edit.replace` cover basic work. Missing: patch-set preconditions, glob/multi-read, binary/media metadata, symbol/diagnostic navigation, and stronger cancellation on search/read. |
| Process tools | [~] useful baseline | `shell.exec` has timeout, bounded tail, and artifact output. It is a one-shot shell string, not a structured run/start/poll/stop process protocol; process-tree isolation remains incomplete. |
| VCS tools | [~] minimal | `git.status` and `git.diff` are confined, bounded, and read-only. `log/show/blame` and structured change review are absent; commit/push must remain higher-risk effects. |
| Runtime control | [~] partial | `context.manage` and `capability.manage` are merged bounded control surfaces. There is no typed TaskAnchor/open-loop control, artifact fetch surface, or task completion proposal. |
| Completion semantics | [ ] missing | The final answer is a normal AssistantMessage and `/done` accepts unrelated free text. `CTX-10` defines the required TaskAnchor-to-CompletionRecord root transfer. |
| Tool-system evaluation | [ ] missing | Unit tests verify individual tools and bounds, but there is no ACI-level comparison against shell-only/current/redesigned surfaces on real coding tasks. |

## Incremental Core migration

Do not create a second long-lived `agent-core` beside `agent-kernel`, and do
not move the actor merely to make the dependency graph look cleaner. Evolve
the existing kernel crate in place, then perform one mechanical rename after
its contents match the name.

Recommended independently compilable slices:

1. Lock current event-order, approval, live/stale effect, capability
   lifecycle, and checkpoint traces with regression tests.
2. Inside `agent-kernel`, isolate `EventAuthority`, `ApprovalAuthority`,
   `EffectAuthority`, and `OutputAuthority` behind the existing facade. This
   first centralizes calls; it is not yet proof that opaque effects are safe.
3. Add `agent-runtime::RuntimeServices` and move system prompt/config,
   ContextEngine scheduling/query rendering, ModelTransport calls, and
   ToolDispatcher lifecycle/surface scheduling out of the kernel. The actor
   still decides every trigger and order.
4. Once the remaining crate is actual authority, atomically rename
   `agent-kernel -> agent-core` and `AgentKernel -> CoreHandle/CoreAuthority`
   in a behavior-free change.
5. Replace opaque/self-declared effects with inspectable `EffectIntent`,
   Core-issued leases, brokered commit, `EffectReceipt`, and output/resource
   enforcement. Process capabilities remain disabled until this is real.
6. Route post-start capability mutations through the actor safe point, then
   split Core admission/grants/activation/quarantine from Runtime
   start/stop/load/active/surface state.
7. Extract stateless shared bootstrap from TUI for TUI/CLI/eval; it wires one
   actor and never owns task/turn state.

Responsibility tests for the final boundary:

```text
Core decides whether/within what hard limit an operation may happen.
Runtime decides what task step happens next and what fits inside that limit.
```

- Event timing remains Runtime-owned; Core assigns identity, persists,
  barriers, and broadcasts.
- The operation generation remains Actor-owned; Core validates that the
  supplied authorization lease is current before commit.
- TaskAnchor, Episode, Focus, CompletionRecord, PromptAssembler, context/tool
  selection and model packing never move into Core.
- Core owns hard resource ceilings; Runtime owns adaptive packing within the
  ceiling.
- Capability admission, origin, permission ceiling, grants, activation,
  quarantine and maturity are Core authority. Process start/stop, retry,
  per-tool load/warm/active state and surface construction remain Runtime
  scheduling.
- A RuntimeCheckpoint may request a previous tool surface, but it cannot
  restore an old grant, raise maturity, enable a disabled capability, or
  undo a later quarantine.

## Proposed builtin ACI

The names below describe roles. Do not rename the current tools until the
contract and A/B results justify migration.

### Tier 0: always-available runtime controls

These are trusted runtime operations, not ecosystem capabilities:

- `context.manage`: lifecycle hint/tag/lease/collect and deliberate recall;
- `capability.manage`: bounded search/inspect/load/unload of optional tools;
- `task.manage` (proposed): inspect/propose TaskAnchor, plan, open-loop, and
  acceptance-state updates; runtime events remain the authority;
- `task.complete` (proposed): submit a structured completion proposal with
  output/evidence/artifact/open-loop references; runtime verification and
  durable commit create the CompletionRecord;
- `artifact.fetch` (proposed): bounded range/excerpt access to an artifact by
  stable reference without copying it into a new observation.

TaskAnchor must also update automatically from trusted runtime events. These
controls must not turn long tasks into a sequence of user confirmations.

`task.manage` and `task.complete` are Runtime-owned directives, not ordinary
capabilities. An external plugin cannot acquire task-authority merely by
declaring a permission. Proposed completion shape:

```text
CompletionProposal {
  base_anchor_revision,
  proposed_outcome: Succeeded | Partial | Failed | Abandoned,
  exact_final_response,
  criterion_evidence_refs,
  artifact/effect refs,
  unresolved_open_loops
}
```

The model may propose outcome/evidence, but Runtime verifies referenced ids
and the evaluator determines actual criterion status. A false `Succeeded`
proposal becomes Partial/Failed; it never overrides failed tests. The actor
stores `exact_final_response` before context truncation, durably prepares and
commits the CompletionRecord, then transfers GC roots and publishes the
response. This is automatic completion, not a user approval request.

For migration, a plain final assistant response remains a turn response but
does not silently manufacture a successful CompletionRecord. The legacy
`/done <free text>` path should be deprecated only after the structured path
and recovery tests exist.

### Tier 1: stable coding primitives

Keep a small complete set available or cheaply loadable:

- workspace list/read with range, pagination, line numbers, content digest,
  and stable file revision;
- bounded text search with path/glob filters, clear truncation, paging, and
  artifact spill;
- transactional patch application with file-revision preconditions, unified
  diff preview, atomic multi-file commit, and rollback evidence;
- explicit create/replace for cases a patch cannot express, with input caps;
- sandboxed process execution with argv/cwd/env/timeout/cancel and bounded
  stdout/stderr artifacts; keep raw shell as a controlled escape hatch;
- read-only version-control status/diff plus bounded history/object inspect.

### Tier 2: optional first-party tools

Load these when a task demonstrates a need; do not pay their schema/context
cost on every round:

- symbol/definition/reference/diagnostic navigation backed by LSP or a local
  index;
- persistent process sessions (`start`, `poll`, `send`, `stop`) for servers,
  watchers, and long test suites;
- structured test/build/lint adapters that normalize diagnostics while still
  preserving raw logs as artifacts;
- read-only web/document fetch where product policy allows it;
- advanced VCS inspection and workspace checkpoint/revert.

The process contract should distinguish at least:

- read-only source + writable scratch/build cache for normal build/test;
- copy-on-write workspace overlay whose resulting diff is reviewed and
  committed as one Effect for formatters/code generators;
- explicitly granted direct/external execution only when confinement or an
  effect broker can account for the wider mutation surface.

This avoids pretending an arbitrary shell command can be made transactional
merely by labelling `shell.exec` as `ProcessExecution`.

### Tier 3: ecosystem capabilities

Keep Git hosting, browsers, databases, cloud/deployment, ticketing, email,
design tools, telemetry vendors, and domain workflows out of the default
surface. Discover and load them through the capability catalog, under the
task's authority envelope.

## Tool contract v2

The current `ToolSpec { name, description, input_schema, risk }` is too weak
for a safe modular ecosystem. Model-visible schema and host-only enforcement
metadata should be separate so security detail does not consume prompt
tokens or become model-editable:

```text
ModelToolSpec
  stable name, concise description, bounded input schema

HostToolPolicy
  version/owner/source/maturity
  output/effect/permission/resource/context/audit contracts
```

Together they should cover:

```text
ToolIdentity
  stable id, version, owner/package, source, maturity, compatibility

ModelContract
  input schema and description/schema token cost
  model-output byte/token cap, artifact-spill and paging policy

AuthorityContract
  required PermissionSet, operation/effect kinds, workspace/network scope
  risk lower bound derived by Core, credential and secret access policy

ExecutionContract
  timeout, cancellation, concurrency, retry/idempotency, determinism hints
  process/runtime class, health and lifecycle semantics

ContextContract
  transient vs persisted disposition, evidence/artifact refs
  provenance/taint/authority, TaskAnchor/episode relationship

AuditContract
  bounded start/finish/effect events, resource usage, generation and version
```

Rules:

- a tool/capability declaration is a request and scheduling hint, never the
  final security fact;
- Core computes the effective permission/risk from transport, granted
  handles, and the actual `EffectRequest`;
- all model-facing strings have a global enforced cap even when a tool omits
  its local limit;
- large bodies are stored once and referenced, not cloned through events,
  TurnFrame, context, and artifacts;
- cancellation and timeout semantics are part of compatibility, not optional
  documentation;
- every mutation returns durable evidence sufficient to review and recover.

At invocation time, the Core derives a concrete `EffectIntent` from the
host policy plus validated arguments. It includes the operation kind,
targets, reversibility, idempotency/retry semantics, and resource estimate.
Approval/policy matches this concrete intent, never merely the tool name.

Committed effects return an `EffectReceipt` with `NotApplied | Applied |
Unknown`, stable external/change ids, idempotency key, reversibility, and
evidence references. A remote operation with unknown applied state cannot be
blindly retried.

The minimum v2 vocabulary is intentionally small:

```text
ToolRegistration = ModelToolSpec + HostToolPolicy
EffectIntent     = normalized operation/target before execution
AuthorityLease   = Core-issued authority for one operation/generation
EffectRequest    = staged workspace/process/adapter action
EffectReceipt    = NotApplied | Applied | Unknown + durability/evidence
OutputBroker     = the only path from producer output to ToolOutput
```

Do not add a model-facing output schema yet: current providers consume only
name/description/input schema, while the trusted result envelope and output
limits belong in Host policy. The round snapshot keeps the complete
`ToolRegistration`; provider/prompt projection sees only `ModelToolSpec`, and
authorization/execution uses the paired Host policy from that exact snapshot.

Authorization uses two checks but only one policy decision/user interaction:

```text
1. validate arguments + derive a conservative EffectIntent upper bound
2. standing policy/AuthorityGate -> short-lived AuthorityLease
3. prepare using only lease-scoped handles and Core staging; target world unchanged
4. canonicalize actual targets and prove actual intent is within the lease
5. actor checks generation/cancellation; Core commits or rolls back
6. emit EffectReceipt; OutputBroker bounds/spills the final result
```

If preparation discovers a wider target, it rolls back and requires a new
authorization; it never silently widens the lease. The post-prepare check is
not a second confirmation prompt.

For untrusted capabilities, remove/refuse the current direct
`WorkspaceHandle::write` path: mutation is a serializable request to the Host
broker. A process child submits a patch/write request over IPC; it does not
receive an unrestricted RW workspace. An MCP/remote write becomes an opaque
adapter-call Effect whose network request is sent only inside `commit()`.
When the connection fails after sending and the remote result is unknowable,
the receipt is `Unknown`; without an idempotency key it is never retried
automatically.

`OutputBroker` injects trusted call/tool identity, caps summary/content/
metadata/decoded total, validates artifact ownership, stores overflow once,
and emits digest/truncation/resource audit. Producer-created `ToolOutput`
never bypasses it; the actor cap remains defense in depth.

Compatibility order:

1. add DTOs and conservative `ToolSpec -> ToolRegistration` conversion;
2. move surface snapshots to registrations while providers see only model
   projections;
3. broker existing outputs before converting producers to output drafts;
4. run the new AuthorityGate in shadow mode beside legacy ApprovalGate;
5. migrate builtin workspace effects and receipts;
6. disable direct capability mutation and add IPC EffectRequest;
7. migrate sandboxed shell/process and read-only then mutating MCP adapters.

Legacy wildcard/unknown shell or process intents never qualify for automatic
standing-grant execution. Do not add a policy DSL, LLM risk classifier,
generic distributed transaction, automatic arbitrary-MCP writes, WASM ABI,
marketplace, output schema, or learned tool selection in this migration.

## TaskAnchor-driven tool surface

### Delivery boundary of the current slice

This section describes the target architecture, but the current implementation
boundary is narrower and must remain visible in status reports:

| Status | Delivered boundary |
| --- | --- |
| Implemented in the working tree | `TaskToolRequirementSet` is owned by `TaskRecord`, bounded to 32 exact tool names, canonicalized, revisioned and replaced through whole-set CAS. Stale writers and completed-task mutation are rejected; equivalent replacements do not churn the revision. `TaskInfo` exposes revision/count and live restore uses a per-process high-water mark so an older checkpoint cannot create a CAS ABA. |
| Implemented in the working tree | Contracts define `MustSurface`, `PreferSurface`, `KeepReady`, bounded selected/omitted/blocked decisions, source revisions, and a schema-free `ToolSurfacePlanReport`. RuntimeCheckpoint v2 persists task requirements plus focus/surface counters and explicitly rejects v1 rather than silently treating missing authority as empty. |
| Implemented and verified | Runtime-owned `RoundSurfacePlan` is the sole schema-budget projection. Actor tests cover task-demand lifecycle refresh, missing/over-budget Must refusal before provider start, KeepReady prompt exclusion, provider-budget degradation and recovery, one final immutable snapshot, bounded `ToolSurfacePlanned`, `ModelStarted` ordering, monotonic revisions, atomic builtin capture, capability surface-gate serialization, composite common-cut capture, and checkpoint/suspend/restore reconstruction. Full workspace tests and strict Clippy pass. |
| Still future | Process/schema warmth separation (dynamic capabilities do not cool on builtin idle ticks), structured completion controls, CompletionRecord root transfer, and task/episode/operation requirement lifetimes. |

This closes the **TaskToolRequirements/round-surface** slice: typed tool-root
derivation, per-tool capability lifecycle and per-row provenance are verified.

Final review leaves two tool-surface hardening items. Their authoritative issue
descriptions live in `AUDIT_TODO.md` (`CORE-03`, `CORE-09`) rather
than being assigned duplicate defects here:

- live restore's high-water rebase needs a bounded typed commit event; an
  event/barrier failure after restored state becomes visible must fence normal
  mutation as recovery-required;
- every selected/omitted row carries per-row provenance
  (`TaskRequirement` / `DispatcherRequired` / `CatalogLoadedOptional` /
  `Unknown` for legacy rows), so a task-authored `PreferSurface` is no longer
  indistinguishable from a merely loaded catalog optional. Future
  Focus/Active/RecentUse provenance planes remain open.

The adjacent CTX-07 consumption gap is closed: actor packing now produces an
exact bounded `ContextConsumptionAck`, and trimmed/refused/failed/cancelled/
stale operations receive no reinforcement.

Keep the existing five-state lifecycle and immutable per-round snapshot, but
make its roots semantic:

Four orthogonal axes must not be collapsed:

```text
catalog identity       Registered | Removed
authority              Disabled | Enabled | Quarantined + maturity/grants
operational lifecycle  Available | Loaded | Active | Warm | Unloaded
round projection       MustSurface | Selected | Omitted
```

`Loaded` means eligible/ready, not necessarily visible in every request.
`Active` is a hard lifecycle root. `Warm` may retain cheap process resources
without spending schema tokens. Only the immutable round snapshot is the
truth about what the model saw and may call in that round.

```text
always roots     = minimal read/discovery + context/capability controls
task roots       = TaskAnchor.required_capabilities / execution profile
focus roots      = current Episode/Focus phase and entities
execution roots  = Active invocations
short-lived roots= recently successful tools
```

Examples:

- explain/review: current read/list/search controls;
- change/fix: automatically add patch and sandboxed process tools;
- test/verify: keep process plus diagnostic normalizer;
- Git review: add VCS inspection;
- web/domain task: load the matching external capability;
- task completion: transfer completion evidence roots, then cool and unload
  task/focus-only tools.

The current `FocusState.phase` is a free string. A later typed baseline can
start deliberately small:

```text
Explore -> Plan -> Edit -> Validate -> Deliver
                    `-> Blocked
```

Phase is not inferred as a security fact. It only supplies deterministic
default tool demand: Explore prefers read/search, Edit must surface patch,
Validate must surface the sandboxed runner, and Deliver prefers VCS plus
completion evidence. The TaskExecutionPolicy still decides whether a
concrete invocation may execute.

The model should not have to discover and load the editor and test runner for
every ordinary coding task. `capability.manage` is principally the discovery
path for unknown/optional ecosystem capabilities. Task-driven loading must
still respect the schema budget; a required tool cannot be silently trimmed
without an observable blocked/degraded reason.

Use a bounded typed requirement rather than tool names hidden in prose:

```text
CapabilityRequirement {
  requirement_id,
  selector: ToolId | CapabilityId | CapabilityKind,
  demand: MustSurface | PreferSurface | KeepReady,
  lifetime: Task | Episode | Operation,
  reason,
  source_field_id,
  anchor_revision
}
```

The complete selector/lifetime/provenance form above remains the target. The
first slice intentionally stores only `{ exact tool_name, demand, bounded
reason }` under a task-wide revision. It does not yet resolve a capability
kind, carry Episode/Operation lifetime, or claim an Anchor field/provenance id.

The scheduler resolves requirements only against admitted/enabled catalog
entries. A requirement cannot enable a disabled capability or widen the
task's permission policy. `MustSurface` means the current round cannot
correctly perform its step without the schema; `PreferSurface` competes for
round budget; `KeepReady` preserves quick availability without spending
prompt tokens. An unavailable MustSurface capability produces an observable
`CapabilityBlocked` reason and a degraded/alternate plan.

Capability process lifecycle and model-schema lifecycle must be separate. A
process may remain Started/Warm while only the individually required tool
schemas are Loaded. Loading one tool should not automatically expose all 31
siblings from the same package.

One safe-point transition produces a `DesiredToolSurface` with reason and
priority for every entry, then captures:

```text
surface_revision = one monotonic Runtime surface revision
surface_inputs   = builtin_catalog_generation,
                   capability_catalog_generation,
                   task_requirement_revision,
                   focus_revision,
                   execution_policy_revision
```

In the first slice the task-requirement and focus revisions are real runtime
sources; `execution_policy_revision` remains absent rather than using zero as
a fake revision. `ToolSurfaceSnapshot.generation` stays a legacy catalog
display value. It is not the unique identity of a round and must not be reused
as the task or operation generation fence. The source cut is stable: builtin
specs/generation are captured under the registry lock, capability mutation and
capture are serialized by the surface gate, and the composite dispatcher holds
that read gate while taking one atomic base snapshot. The combined values
therefore existed at one common cut without retry or unstable fallback;
concurrency tests cover catalog changes during capture.

That immutable snapshot is used for schema budgeting, prompt construction,
call validation, audit, and execution. MustSurface roots are packed before
PreferSurface/recency candidates. If mandatory schemas alone do not fit, the
runtime refuses/degrades the round explicitly instead of silently unloading
the tool or deleting context until the request happens to fit.

Demand and provenance are separate. The current fallback assigns
`PreferSurface` packing priority to ordinary loaded optional schemas, but a
report row must eventually distinguish at least:

```text
TaskRequirement(task_id, requirement_revision, bounded_reason_ref)
DispatcherRequired
CatalogLoadedOptional
FocusOrEpisode          # later
ActiveOrRecentUse       # later
```

Without this field, “selected because Task Prefer” and “selected because it
happened to be loaded” are not explainable lifecycle decisions.

Round packing is a pure projection:

```text
budget omission != lifecycle unload
```

A schema omitted because one provider round is small remains Loaded/Warm as
appropriate, does not bump catalog generation, and can reappear when Focus or
budget changes. The actor's previous largest-schema `tool_unload` fallback is
now replaced by fail-closed round-local omission; both initial and final
packing use the same classification, and the final snapshot is published once
after packing. Lifecycle changes occur only from explicit controls/events/idle
maintenance. This is the pre-TaskAnchor P0 baseline, not the final selection
policy.

Event algorithm at the actor safe point:

```text
Task requirement CAS-> replace the bounded Task demand revision
Task/Anchor patch   -> later rebuild the complete Task roots
Focus change        -> replace Focus-generation roots
AfterTool           -> Active -> Loaded; refresh use and possibly Focus roots
Task suspend        -> release surface roots; cool; retain Anchor requirements
Task resume         -> reconstruct from Anchor, not transcript/checkpoint surface
Completion commit   -> transfer evidence roots, then release task/tool roots

BeforeModel
  -> run tool GC at the safe point
  -> refresh exact Task-demand lifecycle flags without granting authority
  -> emit bounded transition reasons
  -> pure-pack MustSurface then PreferSurface candidates
  -> capture one immutable snapshot for budget/prompt/validation/execution
```

For this slice, `KeepReady` means schema/catalog-ready and prompt-cold. A
post-GC reload is acceptable because current `load_tool` only restores a cheap
lifecycle flag; it neither starts a lazy capability process nor grants
activation/effect permission. If loading later acquires resources or performs
effects, this shortcut must be replaced by a transactional lifecycle plan.

Safety revocation is the exception to snapshot stability: a capability
quarantined after snapshot capture must still be refused at invocation and
reported as `RevokedAfterSnapshot`.

RuntimeCheckpoint v2 stores the current task-owned requirement sets, focus
revision and last allocated surface revision, while the host checkpoint keeps
the existing admission/activation flags. It does not restore `Active` or a
derived per-round snapshot. Version 1 is explicitly rejected in this prototype
rather than silently manufacturing an empty requirement authority. The future
complete checkpoint must additionally store TaskAnchor requirements/durable
leases and must never let an old activation override a newer quarantine.
Live-restore revision rebasing is implemented, but its commit is not yet
represented by a bounded typed `RuntimeRestored`/`TaskRequirementsRebased`
event. `AUDIT_TODO.md` CORE-03 owns the required audit barrier and
recovery-required failure semantics.

Required properties:

- **First-slice verification:** a core/MustSurface schema is never swept; a
  missing or over-budget MustSurface tool makes the round explicitly
  unsatisfiable; PreferSurface budget omission does not mutate lifecycle;
  KeepReady remains prompt-cold; budget shrink/recovery preserves lifecycle;
  prompt schemas, accounting, validation and invocation use the exact same
  final snapshot.
- **Safety verification:** quarantine after snapshot still wins at invocation,
  and a refused surface emits no `ModelStarted` claim.
- **Snapshot consistency verified:** builtin same-lock capture, capability
  surface-gate serialization and composite common-cut retry keep specs aligned
  with their recorded source generations under concurrent mutation.
- **First-slice hardening still open:** every selected/omitted row
  differentiates task demand from dispatcher/catalog fallback provenance;
  live restore rebase is durably audited.
- **Adjacent context accounting verified:** final actor packing and successful
  model completion commit an exact bounded consumption acknowledgement;
  trimmed or unsuccessful projections are not reinforced.
- **Still future:** loading one external-capability tool does not mark/surface
  siblings; task switch/resume reconstructs complete TaskAnchor + Episode
  demand; completion failure retains roots and successful CompletionRecord
  commit releases them; model-frame consumption must drive typed episode/
  evidence promotion and root release instead of relying only on the current
  next-model-round tool-scope close heuristic.

## Autonomy without approval fatigue

Long-running tasks cannot depend on a user confirming every step. The Core
should evaluate a task-scoped authority envelope and operation properties:

```text
effective decision = policy hierarchy
                   + task standing grants
                   + requested permission
                   + actual effect/scope
                   + sandbox strength
                   + reversibility/recovery
```

Target defaults to evaluate rather than hard-code:

- workspace reads and bounded inspection: automatic;
- confined, journaled, recoverable workspace edits: automatic when the task
  profile grants repository mutation;
- builds/tests inside the sandbox: automatic under explicit resource caps;
- network, credentials, external writes, publication, deployment, money, and
  destructive/irreversible actions: require a standing grant or explicit
  policy; otherwise deny that path without blocking unrelated safe work;
- extensions may contribute `deny`/`ask` constraints but may never add an
  `allow` above administrator/user/Core policy;
- cancellation or a stale generation always prevents an uncommitted effect;
- approval prompts are exceptional escalation points, not the main safety
  mechanism.

This preserves autonomy by replacing repetitive prompts with confinement,
reversibility, standing grants, and deterministic policy.

## Evaluation plan

### ACI comparison

Use the same model, task set, context policy, budgets, and sandbox:

```text
A: shell-only interface
B: current builtin tools
C: proposed minimal ACI with structured patch/process/task completion
D: C + on-demand capability loading
```

Report at least:

- task/acceptance success and completed-task validity;
- total model + tool-schema + manager + compactor/GC tokens per solved task;
- tool calls, invalid calls, wrong-tool corrections, and repeated reads;
- search-to-correct-location rate and edit-to-passing-verification rate;
- model-facing output bytes, artifact spill/fetch ratio, and truncation loss;
- wall time, process launches, timeout/cancellation recovery;
- permission prompts per task, standing-grant use, denied-action recovery;
- stale/unauthorized mutations, rollback failures, and sandbox escapes;
- TaskAnchor retention, final-output/CompletionRecord agreement, and evidence
  traceability;
- run-to-run variance and paired task outcome deltas.

### Tool conformance suite

Every builtin and external tool must pass a shared harness covering:

- schema validation, unknown/oversized arguments, and version negotiation;
- output/artifact caps, binary/invalid UTF-8, extremely long lines, and
  unbounded stderr;
- cancellation before start/during execution/after preparation;
- timeout, process-tree cleanup, concurrency, retry, and duplicate call ids;
- path traversal, symlink/junction escape, option injection, environment and
  credential leakage;
- effect prepare/commit/rollback, stale generation, and durability failure;
- permission under-declaration, quarantine, crash/restart, and health state;
- prompt authority/taint and context disposition;
- deterministic audit reasons and resource accounting.

### Coding workload

Do not rely on one benchmark. Combine tool-focused adversarial fixtures with
real feature, bug, refactor, repository-orientation, test/debug, review, and
from-scratch tasks. Include long episodes so the tool system is evaluated
together with TaskAnchor and continuous context GC.

## Work queue

### Gate 0: approve boundaries and vocabulary

- [ ] **MOD-01** Confirm the four trust rings and the rule that there is one
  orchestrator.
- [ ] **MOD-02** Reserve "composition module/adapter" for operator-trusted services;
  reserve "capability" for runtime-loadable actions/services; define Skill,
  Hook, and Plugin Package separately.
- [ ] **ECO-01** Decide whether Skills and Hooks are first-class contracts now or remain
  package metadata until the base ACI is measured.

### Gate 1: specify the base ACI before adding tools

- [x] **TOOLS-01** Inventory current tool schemas, limits, context dispositions, effects,
  and platform behavior in a machine-readable matrix.
  **Done 2026-08-11** — [`docs/TOOL_INVENTORY.json`](TOOL_INVENTORY.json): the ten
  current tools (core `fs.list`/`fs.read`/`search.grep`, control
  `context.manage`/`capability.manage`, catalog-optional
  `fs.write`/`edit.replace`/`git.status`/`git.diff`/`shell.exec`) with their
  schemas, hard limits (per-tool and global broker caps), context
  dispositions (persist / transient / access-event / directive), effects and
  platform behavior; the code remains authoritative.
- [x] **TOOLS-02** Draft split `ModelToolSpec` / `HostToolPolicy`, concrete
  `EffectIntent`/`EffectReceipt`, and `PermissionSet`/standing-grant
  contracts without
  changing runtime behavior.
  **Done 2026-08-11** — [`docs/ACI_CONTRACT_DRAFT.md`](ACI_CONTRACT_DRAFT.md):
  field-level `ModelToolSpec`/`HostToolPolicy` split (model projection vs
  host enforcement), concrete `EffectIntent` (typed kind/target/
  reversibility/idempotency/resource estimate) and `EffectReceipt`
  (`NotApplied | Applied | Unknown` + durability/evidence), typed
  `PermissionSet` with a deterministic mapping from today's manifest
  permission strings, `GrantSpec` v2 mapped from today's `StandingGrant`,
  and the field conversions pinned to the existing compatibility order.
  Documentation only; no runtime behavior changed. Open questions
  (permission granularity, shell idempotency, upper-bound derivation,
  PermissionSet placement) are tracked in the draft for `TOOLS-03` and
  `MOD-04`/`MOD-05`.
- [ ] **TOOLS-03** Define stable error/result envelopes and global output/artifact limits.
- [ ] **TOOLS-04** Define the conformance harness and A/B/C/D evaluation fixtures first.

### Gate 2: close correctness/security blockers

- [x] **CORE-GATE-01** Close `CORE-01`, `CORE-04`, `CORE-06`, `CORE-07`, `CORE-08`, and `CORE-09` from
  `AUDIT_TODO.md`; external process capabilities remain disabled by default
  until the actual effect/sandbox path passes adversarial tests.
  **Closed 2026-08-11.** All six dependencies are closed (process-boundary
  parity, output broker, cancellation/sandbox, confined directory-handle
  operations, standing grants, and the round-surface slice); external
  (out-of-process) capabilities still enter `Disabled` at registration and
  stay off the surface until an explicit enable, so the default remains
  safe while the M12 wire-level effect broker is finalized.
- [ ] **MOD-03** Separate trusted composition registration from dynamic capability
  registration in names, docs, and authority checks.
- [ ] **MOD-04** Isolate the first real authority slice inside the existing
  kernel crate (effect, approval/policy, output/resource broker, durable
  audit) without moving `RuntimeActor` or creating a second orchestrator.
- [ ] **MOD-04A** Move context/model/tool/config scheduling behind
  `agent-runtime::RuntimeServices`; only then perform the mechanical
  `agent-kernel -> agent-core` rename as a behavior-free change.
- [ ] **MOD-05** Split capability ownership: Core owns admission, grants,
  activation/quarantine, and maturity authority; Runtime owns catalog views,
  load/unload scheduling, active state, and per-round surface snapshots.
- [ ] **COMPOSE-01** Extract reusable application/bootstrap composition from
  `agent-tui` for TUI/CLI/eval while keeping it stateless and actor-free.
- [ ] **ECO-02** Make manifest identity/path/source validation and process stdout/stderr
  accounting non-bypassable.

### Gate 3: complete and test the minimal ACI

- [ ] **TOOLS-05** Add patch-set/file-revision semantics; evaluate against `edit.replace`
  before deprecating anything.
- [ ] **TOOLS-06** Add structured process/session semantics and process-tree cleanup;
  retain shell as a controlled fallback.
- [ ] **TOOLS-07** Add artifact range fetch and consistent result paging.
- [x] **TOOLS-08P** Validate the TaskToolRequirements/round-surface first
  slice: bounded TaskRecord CAS, Must/Prefer/KeepReady packing and degradation,
  lifecycle refresh, bounded decision events, one final snapshot, runtime
  surface revision, and RuntimeCheckpoint v2. This item does not include the
  complete TaskAnchor or completion transaction.
- [ ] **TOOLS-08** Add TaskAnchor-driven tool roots and structured completion controls only with the
  `CTX-10` CompletionRecord transaction and GC root transfer.
- [ ] **TOOLS-08A** Split capability process state from per-tool surface
  state; loading one tool must not expose all sibling schemas, and external
  tools must receive the same root/idle cooling semantics as builtins.
- [x] **TOOLS-08B** The first slice carries separate catalog, task-requirement
  and focus revisions, hard Must/Prefer/KeepReady degradation, atomic builtin
  capture, capability surface-gate serialization, and a composite common-cut
  protocol verified under concurrent mutation. Complete TaskAnchor,
  Episode/Focus and execution-policy sources remain owned by `TOOLS-08`.
- [~] **TOOLS-08C** Bounded round-surface diagnostics, monotonic revisions and
  `ModelStarted` ordering are verified. Add per-row demand provenance so Task
  Prefer and catalog-loaded optional candidates are distinguishable, plus the
  bounded live-restore rebase audit event owned by `CORE-03`.
- [ ] **TOOLS-09** Evaluate local symbol/diagnostic navigation as optional first-party
  tools; do not add embeddings/vector storage.

### Gate 4: extension packaging

- [ ] **ECO-03** Define a versioned plugin package manifest with explicit contributed
  tools, skills, hooks, adapters, dependencies, permissions, schemas, tests,
  and compatibility range.
- [ ] **ECO-04** Add install/inspect/test/enable/disable/quarantine flows; installation
  never implies activation or permission.
- [ ] **ECO-05** Add MCP-like adapter support behind the same capability/effect/output
  boundary; lazily expose schemas through the existing catalog.
- [ ] **ECO-06** Define Skill activation/deactivation and provenance without turning
  instructions into System-authority content.
- [ ] **ECO-07** Define Hook ordering, time/resource bounds, failure policy, and the rule
  that hooks cannot widen permissions or mutate protected state silently.

### Gate 5: evidence gate

- [ ] **EVAL-TOOLS-01** Run A/B/C/D paired coding-agent evaluation with all costs counted.
- [ ] **EVAL-TOOLS-02** Require no regression in task success, zero stale unauthorized effects,
  bounded schema/output cost, and fewer approval interruptions per solved
  task before declaring the ACI stable.
- [ ] **EVAL-TOOLS-03** Promote an optional capability to builtin only after repeated workload
  evidence shows that always-available reliability outweighs schema,
  maintenance, and attack-surface cost.

## External design evidence

The following first-party sources support the architecture shape; they do
not prove that this project's proposed tool set is optimal:

- OpenAI Codex separates MCP-provided tools/context, reusable Skills, and
  installable Plugins that may package Skills and MCP servers:
  <https://learn.chatgpt.com/docs/extend/mcp> and
  <https://learn.chatgpt.com/docs/build-plugins>.
- Claude Code separates builtin tools, MCP, Skills, Hooks, subagents, and
  Plugin packages, with independent permission and sandbox layers:
  <https://code.claude.com/docs/en/tools-reference>,
  <https://code.claude.com/docs/en/plugins-reference>,
  <https://code.claude.com/docs/en/permissions>, and
  <https://code.claude.com/docs/en/sandboxing>.
- Gemini CLI exposes a compact coding tool set and packages MCP, commands,
  context, skills, hooks, subagents, and policy through Extensions. Its
  policy hierarchy is especially relevant to non-interactive long tasks:
  <https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/tools.md>,
  <https://github.com/google-gemini/gemini-cli/blob/main/docs/extensions/reference.md>,
  and
  <https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/policy-engine.md>.
- OpenHands uses a typed Action-to-Observation tool boundary and separates
  tools, workspace, agent, hooks, plugins, MCP, and runtime sandbox:
  <https://docs.openhands.dev/sdk/arch/tool-system> and
  <https://docs.openhands.dev/sdk/arch/security>.
- SWE-agent's ACI work shows that range viewing, concise search results,
  edit feedback/linting, and command-result formatting materially influence
  coding-agent outcomes:
  <https://swe-agent.com/latest/background/aci/> and
  <https://papers.neurips.cc/paper_files/paper/2024/file/5a7c947568c1b1328ccc5230172e1e7c-Paper-Conference.pdf>.

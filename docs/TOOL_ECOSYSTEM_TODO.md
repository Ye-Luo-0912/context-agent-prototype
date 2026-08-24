# Modular Runtime and Tool Ecosystem TODO

Status: code-grounded discussion draft, 2026-08-13

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

The intended shape combines the **landed PLAT-01 CorePort**, **PLAT-02
semantic contract** and **PLAT-03a1-a4 persistent authority plus builtin
workspace recovery** with the
remaining recovery/adapter/SDK/supervision work (`PLAT-03..07`):

```text
UI / API / user ingress
          |
          v
Platform = agent-runtime ----------------------------------- evolvable
  sole RuntimeActor | TaskAnchor / Episode / Focus
  context/tool scheduling | target unified process/extension lifecycle
          | CorePort                     ^ Platform Protocol
          v                              |
Trusted Core --------------------   Tools / Skills / child Agents /
  grants | leases | effects         MCP / Context services / adapters
  hard bounds | durable audit       (never direct Core clients)
          |
          v
  brokered workspace / process / network / artifact resources

agent-compose / product host = bootloader only, never another orchestrator
```

There remains exactly one turn/task orchestrator: `RuntimeActor`. An
extension can provide a capability, observation, workflow, or lifecycle
interceptor; it cannot create a second task state machine, mutate the
ContextEngine directly, append arbitrary prompt messages, or commit an
  mediated effect outside the Core. Generic process execution is a typed
  non-transactional exception (Core identity before spawn, no rollback of
  child mutations) until M12/M13 close isolation.

Current V1 is one operator-trusted address space. PLAT-01 is landed:
`RuntimeServices` retains only `Arc<dyn CorePort>` as its Core facade; its
scheduling fields and methods, the concrete `RuntimeActor`, `CoreAuthority`,
and Core event/approval/effect/output components are private. Capability/plugin
admission and state primitives remain explicit public Core contracts used by
their registries. `RuntimeServices` constructors and `spawn_runtime` remain
public trusted-composition seams. Effect commit requests carry
run/turn/operation/generation/lease identity, and Core validates its run and
issued lease. Core now also owns a monotonic authority epoch;
Runtime requests CAS advances and retains only a mirror, while Core rejects
stale dispatch and commit. Cancellation advances the fence before any await or
cleanup. A dependency conformance test enforces the production graph. The
landed bounded in-memory Core registry additionally binds a canonical argument
digest and Core-issued effect id to each operation, prevents same-process
duplicate dispatch/commit, retains every unresolved operation and distinguishes
recent/expired/unseen queries. PLAT-03a3 persists epoch and full operation
transitions journal-first behind a checksummed `sync_all` barrier. PLAT-03a4
preallocates the Core `EffectId`, journals exact builtin workspace mutation
evidence and reconciles it at startup; unresolved, unmanaged or ambiguous
effects raise the `RecoveryRequired` mutation fence. Generic shell/process
spawn/exit recovery is landed and those tools fail closed without Core
identity before spawn; out-of-process capability/MCP invoke recovery
is landed, and protocol
process ownership remains split across adapters. PLAT-02 defines the common semantic
contract, but existing adapters do not yet carry it and the SDK is not
implemented. These are structural and same-process stale-work constraints, not
crash exactly-once or an unbypassable same-process security boundary.

The builtin tool set is part of the agent-computer interface (ACI), not just
an incidental collection of helpers. It must be designed and benchmarked as
a product surface because tool schemas, feedback format, output bounds, and
failure semantics directly change model performance.

## Trust rings and ownership

### Ring 0: Trusted Core

The target Core is intentionally small, stable, and unavailable to
model-authored extensions. It must own mechanisms whose compromise invalidates
every higher layer:

- permission and standing-grant evaluation;
- effect preparation/commit/rollback and generation fences;
- sandbox/process/network authority;
- output, time, process, memory, and tool-schema budgets;
- durable events, checkpoint barriers, recovery fencing, and audit identity;
- capability identity, signature/source, activation, quarantine, and
  resource enforcement;
- runtime integrity and the non-bypassable policy envelope.

Today Core centralizes these mediated paths behind the landed `CorePort`, owns
the persistent authority epoch/operation journal, and reconciles exact builtin
workspace evidence at startup. Its authority registries are stateful; "turn-
stateless" means no TaskManager, turn loop or prompt frame, not no state.
`PLAT-03` now has strict query/cancel DTOs, an authorized
transport-independent router, WAL-first acceptance publication and an
actor-owned control seam. In-process authenticated operation-control session
installation is landed. Framed JSON-lines operation-control over an
inherited-pipe analogue (`FramedProtocolSession` + the authenticated adapter)
is landed; Named Pipe/UDS remain PLAT-08. Out-of-process capability/MCP
invoke recovery is landed. A future HTTP/gRPC broker must reuse the same
reserved/dispatch/ack barrier and must not add a second orchestrator.

Core policy must derive authority from the operation and granted handles,
not trust a tool's self-declared risk label. An extension may request less
authority; it can never grant itself more authority.

### Ring 1: Evolvable Platform / Runtime

`agent-runtime` remains the only orchestrator and owns adaptive policy:

- the actor turn state machine, lifecycle transitions and Core-epoch mirror;
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
adapter boundary. Trusted in-process capabilities may receive permission-
scoped `WorkspaceHandle`/`ArtifactHandle` views; isolated code receives
brokered operations and refs. It returns typed values,
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

## Agent OS platform contract and transport

The module boundary follows an OS-like rule:

> Extensions request Platform services; Platform consults Core authority.
> Extensions never import, connect to, or receive handles to Core.

This is not a microservice mandate. Split a component behind wire protocol
when it is untrusted, independently versioned, written in another language,
long-lived outside the run, or needs crash/sandbox isolation. Keep trusted
hot-path implementations in-process when direct typed calls give better
latency and transaction boundaries.

### CorePort versus Platform Protocol

- **CorePort (landed in `PLAT-01`)** is the narrow authority interface from
  the sole RuntimeActor to Core: events, approval, bounded tool execution,
  context acknowledgement/query, checkpoint/restore, and effect
  commit/rollback. Commit requests carry run/turn/operation/generation/lease
  identity; Core validates run, its issued lease and its independently owned
  current authority epoch. Runtime requests CAS advances and keeps only
  a mirror. V1 keeps it in-process. A future hardened Core
  process is evidence-gated by the Self-Iteration threat model and must not
  gain a TaskManager or turn loop.
- **Platform Protocol semantics (landed in `PLAT-02`)** define the stable
  envelope/identity/error contract for Tools, executable Skill workers, hooks,
  child Agents, MCP servers and isolated context/model adapters. It has typed
  namespaces over one bounded envelope; it is not a generic event bus and
  does not pretend every resource is a Tool.
- **Platform SDK (planned by `PLAT-07`)** exposes the least-authority client surface over that
  protocol. Extension packages depend on the SDK/protocol, not
  `agent-core`, `agent-runtime` internals, raw `ContextEngine`, workspace or
  journal objects.

The common envelope must distinguish physical delivery from logical work:

```text
protocol/version/features/schema_digest
message_id / request_id
call_id / operation_id / attempt / effect_id
run_id / task_id / turn_id / scope_id
deadline_remaining / causation_id / correlation_id / trace context
authority lease id + generation (opaque/minimal; never a new grant)
bounded typed payload or artifact ref
```

`request_id` pairs one exchange; `call_id` identifies the model call;
`operation_id` survives retry/reconnect; `effect_id` identifies one proposed
world mutation; trace ids are observability only. Reusing one `operation_id`
with a different argument digest is a protocol violation.

### Reliability and permission invariants

Do not promise transport-level exactly-once. Messages may repeat. The landed
bounded registry is linked to the builtin workspace mutation journal; other
effect kinds need equivalent durable evidence. The landed query/cancel DTOs,
authorized router, WAL-first acceptance event and actor seam preserve
`ExpiredOrPossiblySeen` and exact Core truth; concrete authenticated adapters
still need to consume that seam.
Only a broker with a durable idempotency
barrier may promise at-most-one commit. Ambiguous or unmanaged crash windows
remain unresolved behind `RecoveryRequired` and must never be blindly replayed
under a new id.
Terminal invocation/effect states are monotonic, cancellation is a request
plus generation fence, and late results cannot commit.

Effective authority is always an intersection:

```text
manifest request
  ∩ installed Core grant
  ∩ run/task policy and resource scope
  ∩ short-lived invocation lease
  ∩ effect-specific commit decision
```

OS peer identity, process ownership and transport ACL only admit a connection.
They never grant an operation. The target trusted boundary canonicalizes
call/tool/artifact identity from the request and registered record, validates
bounded arguments/result envelopes, and does not trust producer-returned
identity or self-declared risk. Full JSON-schema/output-schema support is not
part of the current contract and must not introduce an unbounded validator.

### Minimal model-visible runtime facts (`TOOL-ENV-01`)

The active tool schemas already tell the model which tools exist. Do not copy
their names or usage instructions into the standing prompt. The missing
contract is smaller: the model must know which host it is operating on and the
exact dialect of any raw shell tool before it constructs a command.

Trusted composition should capture one bounded, revisioned
`RuntimeFactsView` at startup and let `PromptAssembler` place its stable block
immediately after System Policy on every model request. The initial profile is
limited to:

```text
runtime_facts/v1
platform: windows 11 | ubuntu 24.04 | macos 15 | <normalized unknown>
architecture: x86_64 | aarch64 | <normalized unknown>
workspace: relative paths; markers: [.git, Cargo.toml, ...]
```

Linux reports distribution plus product release when available (for example
`ubuntu 24.04`), not merely the word `linux`; an unknown distribution or
release is reported as `unknown`, never guessed. Windows reports the product
release (for example `windows 11`). Full Windows build numbers, kernel strings,
host/user names, environment variables, absolute workspace paths, PATH
inventories and installed-program lists stay out of the default block. Exact
build/toolchain versions are queried only when a concrete compatibility check
requires them. Project markers are obtained through confined reads, are
bounded and sorted, and say explicitly when no known manifest is present.

Shell identity belongs to the selected tool schema, not to Runtime Facts.
`shell.exec` must state the exact fixed dialect selected for the run, such as
`PowerShell 7.x`, `Windows PowerShell 5.1`, `cmd.exe`, or `POSIX sh`; it must
not advertise the generic word "shell" while dispatching a different grammar.
On Windows the target product default is an explicitly detected/pinned
PowerShell 7 (`pwsh`) because it gives one modern, well-known command language.
If it is unavailable, composition may select Windows PowerShell 5.1 or
`cmd.exe`, but the fallback must be visible in the schema and must not change
mid-run. `process.run` remains the preferred no-shell argv path for direct
executables.

The facts block is system-owned, cache-stable and charged as a fixed prompt
layer; it never enters `ContextEngine`, transcript history or GC. V1 hard caps:
1 KiB UTF-8 total, at most 16 workspace markers and 64 bytes per marker.
Workspace-marker refresh happens at actor safe points after a committed
workspace mutation and after a successful `shell.exec` / `process.run`;
OS/shell identity stays immutable for the run.

### Measured tool-reliability preflight (`TOOL-ENV-01` → `TOOL-ERROR-01`)

**CLOSED in code 2026-08-17** (Gate 3 checklist `[x]`). The "Current code
baseline" table below was stale `[ ]` / "open" until 2026-08-21 (docs-only).
Another Context Bench live wave is a later-milestone frozen-cell rerun; it
is not v0 engineering and does not close M15. Spec that was closed:

The 2026-08-17 Context Bench wave cannot cleanly attribute extra model rounds
to context policy while preventable tool failures dominate the traces. Before
another live A/C wave, close the following product-quality preflight with
scoring frozen (same order as `docs/ROADMAP.md` tool-quality preflight):

1. **Platform and project contract (`TOOL-ENV-01`).** Land the bounded facts
   layer above and bind `shell.exec` to one disclosed dialect/version. A model
   must not infer POSIX syntax on `cmd.exe` or run a project-specific build
   command against an absent manifest.
2. **Revision-aware editing (`TOOL-EDIT-01`).** Keep exact matching and staged
   effects, but let `edit.replace` accept the `fs.read` revision. Refusals must
   distinguish stale revision, no exact match and ambiguous match and return
   only bounded current-revision/candidate context. `edit.patch` remains the
   preferred revision-aware multi-hunk operation. Never fuzzy-apply a guessed
   anchor.
3. **One model-visible workspace view (`TOOL-VIEW-01`).** Use the same bounded
   visibility policy across list/read/search/code navigation. Ordinary file
   tools must not expose `.focus-agent` or raw `.git` internals; sealed output
   is fetched through `artifact.read` and VCS state through `git.*`. Runtime
   Facts supplies real project markers; missing files/manifests stay missing.
4. **Typed recovery feedback (`TOOL-ERROR-01`).** Project trusted bounded
   failure classes and the minimum corrective fact into tool results. A failed
   tool normally needs another model turn; do not hide that treatment cost and
   do not add blind automatic retries or command translation.

Acceptance is the same frozen wave rerun plus focused tool regressions:
wrong-dialect, missing-marker and stale-edit failures must fall; first-attempt
edit success must rise; model-visible `.focus-agent` access must be zero; and
all residual product/task/provider/infrastructure failures remain separately
counted in rounds, latency, tokens and task outcome. Only after this preflight
may the Context Bench be used to estimate the incremental value of `CTX-11`.

### One protocol, selectable transports

| Boundary | Default |
| --- | --- |
| Trusted same-process implementation | direct trait / actor message |
| Platform-owned one-to-one child | inherited anonymous pipes; dedicated protocol handles |
| Persistent/reconnectable local service | Windows Named Pipe / Unix Domain Socket with ACL/peer checks |
| Remote/ecosystem integration | MCP, HTTP or gRPC adapter terminating at Platform |
| Large/binary output | immutable artifact/ref, never inline control data |

The current stdio backend is already an OS anonymous-pipe channel. Named
Pipe/UDS are for independent lifecycle, reconnection and peer admission, not
an assumed performance win. Standard output/error remain bounded logs; the
protocol should eventually use dedicated handles. JSON remains acceptable
until measurement proves codec cost material; length framing, binary codecs,
streaming and multiplexing are later transport choices, not prerequisites for
the authority contract.

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
| Single orchestrator | [x] | `RuntimeActor` owns turn state; `agent-core` remains turn-stateless. Preserve this boundary. |
| Narrow Core boundary | [x] PLAT-01 + [~] PLAT-03 | `RuntimeServices` holds only `Arc<dyn CorePort>` as its Core facade; its scheduling state/methods and the concrete actor/Core implementations are private. Capability/plugin admission/state remain explicit Core primitives. Core owns the epoch and bounded registry; composition injects the persistent authority journal and builtin workspace reconciler. Exact workspace evidence is reconciled on restart, and RuntimeCheckpoint v4 validates a stable WAL-prefix marker; unmanaged or ambiguous effects remain fenced. Runtime requests CAS advances and remains the sole orchestrator. `agent-conformance` enforces forbidden production dependency paths. The authorized transport-independent operation router, WAL-first acceptance publication, WAL compaction, generic process spawn/exit recovery, in-process authenticated operation-control sessions, framed JSON-lines operation-control over an inherited-pipe analogue and out-of-process capability/MCP invoke recovery are landed; Named Pipe/UDS remain PLAT-08. PLAT-04 common-contract proof is landed; adapter envelope migration remains PLAT-07. |
| Typed composition modules | [x] / needs naming | `agent-runtime/src/host.rs` publishes context/model/tool/approval/event/artifact services through a typed registry. This is a trusted composition plane, not the boundary for arbitrary model-authored plugins. |
| Dynamic capability catalog | [x] / lifecycle split remains | `CapabilityRegistry` validates/caches declared tool schemas, prevents name shadowing, tracks activation/maturity, and merges them through one dispatcher. Lifecycle and idle cooling are per tool, so loading one tool never surfaces siblings; owner process start/stop is separate. Invocation/effect lifecycle still needs its own typed axis. |
| Bounded active tool surface | [x] | Bounded task-owned exact-tool requirements, `MustSurface`/`PreferSurface`/`KeepReady`, the runtime-owned `RoundSurfacePlan`, bounded selection/omission/block reports with per-row provenance, source revisions, and a monotonic round `surface_revision` are implemented and verified. A typed-root policy derives family roots from the TaskAnchor/focus/active-call state at BeforeModel. RuntimeCheckpoint v4 persists requirements, anchors and counters rather than a derived snapshot, and references Core authority through a verified WAL-prefix marker. |
| Builtin effect staging | [x] baseline | `fs.write` and `edit.replace` return `PreparedEffect`; Runtime validates its epoch mirror and Core independently validates the authority epoch before commit/rollback. |
| Generic shell effects | [~] controlled escape hatch | `shell.exec` can mutate the real workspace and cannot become a rollback-able prepared effect for an arbitrary command. It is therefore separately permissioned, bounded, whole-tree cancelled and audited; standing grants are intent-constrained. M13 OS confinement and the real-workload evaluation gate remain mandatory before autonomous use. |
| External effect enforcement | [~] fail-closed containment / M13 residual | Non-empty process `WireEffect`s are rejected before staging until actual intent can be bound to PLAT operation/effect identity and proved within the lease. Empty-effect/value responses remain usable and request-owned output identity is canonicalized. Direct child syscalls are not protocol effects: Linux write confinement, Linux TCP deny (ABI v4+), and Windows Low-IL write confinement are landed, but direct reads, UDP/raw/pathname-Unix sockets and Windows network remain the `CORE-01`/M13 residual. |
| Extension sandbox | [~] M13 residual | Env scrub, private cwd, bounded stderr, deadlines, whole-process-tree termination, Windows Job quotas (including wrap 512 MiB and NORMAL priority), Unix rlimits (CPU/process/AS/FSIZE/NOFILE/CORE plus Linux NICE/RTPRIO/`no_new_privs`), brokered filesystem reads, deny-by-default brokered network, Linux Landlock write confinement, Linux Landlock TCP deny (ABI v4+), Linux Landlock device-ioctl deny (ABI v5), Linux Landlock signal scope (ABI v6) and Windows Low-IL write confinement are implemented. Direct reads, UDP/raw/pathname-Unix sockets, Windows network confinement and I/O bandwidth limits remain open. TTY detach, `HANDLE_LIST`, and Job UI restrictions stay skipped. |
| Permission model | [~] bounded baseline | Registration validates the known permission vocabulary, derives minimum risk, and Core stores the accepted grant rather than trusting a live manifest. Standing grants narrow concrete effect intent. The vocabulary remains coarse and the landed PLAT-02 identity/error vocabulary is not yet the recoverable per-operation Platform authority of `PLAT-03/04`. |
| Output contract | [x] model boundary / protocol caps closed | The trusted output broker caps model-facing fields when wired and the actor keeps a last-line guard; process streams use bounded fragments and an 8 MiB per-invocation/session artifact cap while draining overflow. Zero-byte captures return an explicit no-output terminal message without publishing an empty artifact ref; non-empty/truncated captures retain sealed artifacts. Process responses take `call_id`/`tool_name` from the trusted request. `PLAT-00` caps every outbound/control/decoded frame and constrains large broker values (256 KiB bounded reads with truncation metadata). Artifact refs are capped identity locators (`artifact://v1/<run>/<owner>/<digest>`); live captures use an explicit draft until seal. Parse-time decoded JSON DOM budgets are landed. Remaining `PLAT-04` work is landed (JCS, legacy negotiation, shared fault matrix). Adapter envelope migration remains `PLAT-07`. |
| File/navigation tools | [x] `TOOL-VIEW-01` | Ordinary `fs.list` / `fs.read` / `search.grep` / code tools hide `.focus-agent` and raw `.git`. Sealed evidence stays on `artifact.read`; VCS on `git.*`. Missing paths return a bounded parent hint. Glob/multi-read, binary/media metadata, and read-tool cancellation remain optional later gaps. |
| Edit reliability | [x] `TOOL-EDIT-01` | `edit.replace` accepts optional `base_revision` from `fs.read`; refusals are typed `stale_revision` / `no_exact_match` / `ambiguous_match` with at most three candidate regions. Matching stays exact; `edit.patch` remains the multi-hunk path, requires explicit `replace` / `insert_before` / `insert_after` intent on the model surface, preserves insertion anchors, exposes only unique exact anchors, keeps omitted op/ordinal selection parser-only, and preserves both ends of a bounded success echo. |
| Tool failure feedback | [x] `TOOL-ERROR-01` | Trusted `metadata._runtime` (`failure_class` + recovery hint) and a model-visible `runtime_failure:` header. No blind retry or cost exclusion. Failed execution results stay TurnFrame-only. |
| Process tools | [x] dialect disclosed / M13 residual | `shell.exec` binds one disclosed dialect for the run (Windows prefers `pwsh`, then Windows PowerShell 5.1, then `cmd.exe`; Unix POSIX `sh`). `process.run` is the no-shell argv path; `process.session` is start/poll/stop. Host-owned `verify.run` recipe ids are schema values, not tool-catalog names and never require `capability.manage` load. Raw shell remains a non-transactional fallback. OS isolation residual is M12/M13, not this row. |
| Runtime facts | [x] `TOOL-ENV-01` | `runtime_facts/v1` sits after System Policy (≤1 KiB, ≤16 confined markers). Normalized OS product/release, architecture, workspace-relative contract. Facts never enter ContextEngine, transcript, or GC. |
| VCS tools | [~] minimal | `git.status` and `git.diff` are confined, bounded, and read-only. `log/show/blame` and structured change review are absent; commit/push must remain higher-risk effects. |
| Runtime control | [x] bounded baseline | `context.manage`, `capability.manage`, typed TaskAnchor patches, `task.complete`, and `artifact.read` exist. Federated resource discovery and managed-child controls are the next experimental surfaces. |
| Completion semantics | [x] typed baseline | `task.complete` commits a task-owned `CompletionRecord` whose ref/digest identify the bounded completion summary; with artifact storage wired, the complete assistant response is retained separately by artifact ref. Completion transfers roots atomically. A raw-body digest and typed EpisodeOutcome remain context work. |
| Tool-system evaluation | [~] live paired harness | The conformance harness and A/B/C/D scripted coding fixtures compare shell-only/current/redesigned surfaces and hidden outcomes. `--compare-live` runs those fixtures through a real tool-capable model. M15 remains open: no 300×3 non-inferiority result with total cost/latency. |

## Incremental Core migration

Do not create a second long-lived `agent-core` beside `agent-core`, and do
not move the actor merely to make the dependency graph look cleaner. Evolve
the existing kernel crate in place, then perform one mechanical rename after
its contents match the name.

Recommended independently compilable slices:

1. Lock current event-order, approval, live/stale effect, capability
   lifecycle, and checkpoint traces with regression tests.
2. Inside `agent-core`, isolate `EventAuthority`, `ApprovalAuthority`,
   `EffectAuthority`, and `OutputAuthority` behind the existing facade. This
   first centralizes calls; it is not yet proof that opaque effects are safe.
3. Add `agent-runtime::RuntimeServices` and move system prompt/config,
   ContextEngine scheduling/query rendering, ModelTransport calls, and
   ToolDispatcher lifecycle/surface scheduling out of the kernel. The actor
   still decides every trigger and order.
4. Once the remaining crate is actual authority, atomically rename
   `agent-kernel -> agent-core` and `AgentKernel -> CoreAuthority`
   in a behavior-free change. **Landed 2026-08-12** (the crate is
   `agent-core`; `CoreAuthority`/`CoreAuthorityConfig` are the facade and
   its configuration; the runtime derives the facade from
   `RuntimeServices`).
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
- **Current PLAT-03 boundary:** a1-a4 provide Core-owned epoch/operation/effect
  identity, journal-first persistence and exact builtin workspace recovery;
  generic shell/process spawn/exit recovery is landed; out-of-process
  capability/MCP invoke recovery is landed. Unmanaged HTTP/gRPC brokers still
  fail closed. RuntimeCheckpoint v4 now
  validates a stable authority-WAL prefix before restore without rewinding
  Core. Typed query/cancel routes and the exact-current-tool RuntimeActor seam
  are landed; remaining payload/conformance work is in the authoritative
  `PLAT-04` work item below. Task/turn scheduling never moves into Core.
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

### Tier 0: Runtime-owned controls (visibility is demand-driven)

These are trusted runtime operations, not ecosystem capabilities:

- `context.manage` (landed, catalog-cold until typed evidence demand):
  deliberate catalog recall and bounded context directives;
- `capability.manage` (landed, always visible): bounded
  search/inspect/load/unload of optional tools;
- `task.manage` (proposed next long-task slice): CAS-propose bounded
  TaskAnchor plan/open-loop/next-action updates; runtime events remain the
  authority and user constraints stay on the boundary/approval path;
- `task.complete` (landed, catalog-cold until closure intent/requirement):
  submit a bounded completion summary; Runtime attaches owned
  output/verification refs and durable commit creates the CompletionRecord;
- `artifact.read` (landed, always visible): bounded range/excerpt access to an
  artifact by stable reference without copying the full body into a new
  observation.
- `runtime.search` (proposed experiment): federated read-only discovery across
  provider-owned Context/Tool descriptors before widening to other resource
  kinds; resolving a result never implies admission, loading, or invocation;
- `agent.manage` (future, evidence-gated): bounded discovery, assignment,
  wait/status/cancel, and handoff collection for Runtime-owned child workers.

ExecutionState already updates automatically from trusted runtime events.
TaskAnchor semantic progress remains explicit: the proposed `task.manage`
call updates only bounded autonomous fields at semantic safe points. These
controls must not turn long tasks into a fixed plan or a sequence of user
confirmations. See
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md).

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

This contract is the `tool/effect` namespace of Platform Protocol, not a
second wire protocol. Platform-level ids, recovery, framing and transport are
owned by `PLAT-02..08`; this section owns tool registration, effect intent,
lease, receipt and output semantics.

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
5. actor checks its epoch mirror/cancellation; Core independently checks its authority epoch and commits or rolls back
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

This section records the original TaskToolRequirements slice. The current
implementation has since extended it with complete TaskAnchor and completion
semantics; keep the original slice boundary visible for test ownership:

| Status | Delivered boundary |
| --- | --- |
| Implemented in the working tree | `TaskToolRequirementSet` is owned by `TaskRecord`, bounded to 32 exact tool names, canonicalized, revisioned and replaced through whole-set CAS. Stale writers and completed-task mutation are rejected; equivalent replacements do not churn the revision. `TaskInfo` exposes revision/count and live restore uses a per-process high-water mark so an older checkpoint cannot create a CAS ABA. |
| Implemented in the working tree | Contracts define `MustSurface`, `PreferSurface`, `KeepReady`, bounded selected/omitted/blocked decisions, source revisions, and a schema-free `ToolSurfacePlanReport`. RuntimeCheckpoint v4 persists task requirements, TaskAnchor and focus/surface counters and cross-checks Core authority; older versions are explicitly rejected rather than silently treating missing authority as empty. |
| Implemented and verified | Runtime-owned `RoundSurfacePlan` is the sole schema-budget projection. Actor tests cover task-demand lifecycle refresh, missing/over-budget Must refusal before provider start, KeepReady prompt exclusion, provider-budget degradation and recovery, one final immutable snapshot, bounded `ToolSurfacePlanned`, `ModelStarted` ordering, monotonic revisions, atomic builtin capture, capability surface-gate serialization, composite common-cut capture, and checkpoint/suspend/restore reconstruction. Full workspace tests and strict Clippy pass. |
| Implemented after the first slice | Per-tool capability cooling without sibling surfacing, full TaskAnchor tool roots, structured completion controls, and CompletionRecord root transfer. Typed EpisodeOutcome and distinct invocation/effect lifecycle remain future. |

This closes the **TaskToolRequirements/round-surface** slice: typed tool-root
derivation, per-tool capability lifecycle and per-row provenance are verified.

Final review closed the two first-slice hardening items through `CORE-03` and
`CORE-09`:

- live restore now keeps the actor recovery-fenced while the host applies a
  fail-closed capability-state meet, then durably emits bounded
  `RuntimeRestored`; barrier failure keeps the fence;
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

RuntimeCheckpoint stores the current task-owned requirement sets, focus
revision and last allocated surface revision, while the host checkpoint keeps
the existing admission/activation flags. It does not restore `Active` or a
derived per-round snapshot. Version 1 is explicitly rejected rather than
silently manufacturing empty authority. RuntimeCheckpoint v4 includes the
complete TaskAnchor; two-stage live restore rebases revisions, never lets an
old Enabled snapshot lift a live Disabled/Quarantined capability, and releases
the recovery fence only after durable `RuntimeRestored`.

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
- **First-slice hardening verified:** every selected/omitted row
  differentiates task demand from dispatcher/catalog fallback provenance;
  live restore rebase and capability application are durably audited/fenced.
- **Adjacent context accounting verified:** final actor packing and successful
  model completion commit an exact bounded consumption acknowledgement;
  trimmed or unsuccessful projections are not reinforced.
- **Implemented adjacent lifecycle:** external capability tools now cool and
  load per tool without surfacing siblings; TaskAnchor tool roots, task
  switch/resume reconstruction, CompletionRecord root transfer, and exact
  model-frame consumption acknowledgement are present.
- **Still future:** split catalog residency from invocation state, extend
  acknowledgement into typed Episode/evidence promotion, and replace the
  remaining next-model-round tool-scope close heuristic with explicit
  obligation/root-transfer semantics.

## Discovery and managed execution lifecycles

The model may use one tool-like control plane to discover tools, context,
artifacts, tasks, skills, capabilities, and managed collaborators. That does
not make all resources tools internally. The shared abstraction is a bounded
descriptor, typed reference, revision, policy envelope, and observable
lifecycle; each provider retains its own authority and execution protocol.

User input follows the same event/fence/consumption machinery but is not a
`ToolOutput`. It has user authority to steer, interrupt, constrain, or cancel
the active task. A tool or child-agent result is evidence/advice and may only
propose a state patch. The detailed event contract lives in
`CONTEXT_RUNTIME_TODO.md` (`CTX-EVENT-01..03`).

### Keep lifecycle axes separate

The current catalog already supports
`Available -> Loaded -> Active -> Warm -> Unloaded`, and dynamic capability
processes separately track `Stopped/Starting/Started/Stopping/Failed`.
`Active` currently also describes a call in flight. The target should stop
using one value for schema visibility and execution ownership:

| Axis | Purpose | Target states |
| --- | --- | --- |
| Descriptor/schema | Discovery, prompt-surface cost, idle GC | Registered/Available, Loaded, Warm, Unloaded, Retired; quarantine remains Core policy |
| Invocation/effect | One call, approval, cancellation, commit | Prepared, PendingApproval, Running, Committing, Completed, Failed, Cancelled, Stale |
| Hosted process/session | Reusable external resource | Stopped, Starting, Ready, Busy, Stopping, Failed |
| Managed child Agent | Delegated asynchronous work | Allocated, Starting, Running, Waiting, Completed, Failed, Cancelled, Expired, Collected |

GC may cool schemas, stop idle hosts, and archive collected child evidence,
but it must never sweep an active invocation or child lease. Active execution,
TaskAnchor demand, pending effects, and parent open-loop claims are typed
roots; cancellation first fences effects and only then releases resources.

### Child Agent as a tool-like model interface

Expose a small bounded `agent.manage`-style interface rather than injecting
every specialist into the prompt:

```text
search / inspect
spawn(AssignmentCard, budget, permission subset, deadline)
send(child_ref, bounded message or evidence refs)
wait / status / cancel / collect
```

The main model chooses assignments and coordination, but the sole
`RuntimeActor` owns child handles, budgets, cancellation generations, event
ordering, permission narrowing, and parent-state commits. A child cannot
grant itself permissions, write the parent's TaskAnchor, commit a parent
CompletionRecord, load arbitrary capabilities, or modify evaluation/Core
policy. Its full result is stored once as an artifact; the parent receives a
bounded `HandoffCard` plus exact refs and decides what to admit.

This is orchestration through a tool-like ABI, not a second orchestrator. The
first implementation must be a scoped child execution service behind Runtime
and the existing effect/sandbox fences. Broad autonomous multi-agent use
remains disabled until the real evaluation gate demonstrates benefit over
spending the same token/time budget on the single main actor.

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

- [x] **MOD-01** Confirm the four trust rings and the rule that there is one
  orchestrator.
  **Verified 2026-08-12.** The four rings are (1) the trusted core
  (`agent-core`: turn-stateless `CoreAuthority` seams — events, approval,
  effects, output — plus stateful admission/state authorities; owns no turn
  state), (2) the runtime orchestrator
  (`agent-runtime`: `RuntimeActor` owns the turn state machine, task
  manager, scope lifecycle, prompt assembly and effect fence;
  `RuntimeServices` owns scheduling), (3) the trusted composition plane
  (`agent-tui` composition root + `ModuleHost::add_module` /
  `ServiceRegistry::register`: operator-trusted typed services, refused
  after the host started) and (4) the dynamic capability plane
  (`register_capability`, capability registry, process/MCP adapters,
  plugin packages: runtime-loadable, permissioned, out-of-process
  transports pinned to Experimental + Disabled). The one-orchestrator
  rule holds: `CoreAuthority` "owns no turn state" (kernel.rs) and no
  second command loop exists; documented in `docs/ARCHITECTURE.md` §2b.
- [x] **MOD-02** Reserve "composition module/adapter" for operator-trusted services;
  reserve "capability" for runtime-loadable actions/services; define Skill,
  Hook, and Plugin Package separately.
  **Verified 2026-08-12.** Names and authority: `ModuleHost::add_module`
  + `ServiceRegistry::register` publish typed services at composition
  time (trusted core plane, refused after start) while
  `register_capability` accepts dynamic capabilities mid-run (see
  MOD-03); the two planes are distinct entry points with different
  admission. Skill, Hook and Plugin Package are separate manifest
  declarations — `SkillDeclaration` (provenance + activation, ECO-06),
  `HookDeclaration` (ordering/bounds/failure policy/permission subset,
  ECO-07), `PluginPackageManifest` (versioned unit with tools/skills/
  hooks/adapters/dependencies/permissions/tests, ECO-03) — skills and
  hooks are validated metadata, only tools are interpreted. Vocabulary
  recorded in `docs/ARCHITECTURE.md` §2b.
- [x] **ECO-01** Decide whether Skills and Hooks are first-class contracts now or remain
    package metadata until the base ACI is measured.
    **Decision 2026-08-12: Skills and Hooks remain *declared package
    metadata* — they are not first-class runtime contracts yet.** A
    manifest may declare a Skill (`provides: Vec<CapabilityKind>` already
    carries `tool`/`skill`/`service`) and may carry a Hook section, but the
    runtime does not interpret either: a declared Skill never injects
    instructions into context, never starts a process, and adds no
    authority; a declared Hook never fires on lifecycle events. First-class
    status is gated on the evidence gate (Gate 5 / M15): the base ACI was
    only just completed (Gate 3, TOOLS-05..09), so there is no real-load
    measurement yet, and the two open risks that would justify first-class
    mechanics — Skill provenance without turning instructions into
    System-authority content (ECO-06) and Hook ordering/time/resource bounds
    and failure policy (ECO-07) — cannot be validated against workloads
    that do not exist yet. Until then the package manifest (ECO-03) treats
    Skills and Hooks as versioned, validated, source-attributed metadata:
    shape-checked at install, never executed, never permission-bearing. Two
    hard rules already stated in this document stay binding: loading a
    Skill must not implicitly start a process capability, and installing an
    MCP adapter must not implicitly inject its schema catalog into every
    model request.

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
- [x] **TOOLS-03** Define stable error/result envelopes and global output/artifact limits.
  **Done 2026-08-11** — [`docs/TOOL_RESULT_ENVELOPE.md`](TOOL_RESULT_ENVELOPE.md):
  the authoritative specification of the current `ToolOutput` result
  envelope (field semantics and hard caps), the `AgentError`/
  `EffectCommitError`/`OperationOutcome` error envelope, the complete global
  output-limit list (broker/actor/provider caps with enforcement points),
  the per-tool limit matrix (matching `TOOL_INVENTORY.json`), the artifact
  contract (location, `artifact://` refs, spill-once, boundedness,
  ownership), and the two truncation-marker formats. Documentation only; no
  runtime behavior changed. Open questions (structured error codes on
  `ok:false`, marker-assertion granularity for the harness, artifact TTL)
  are tracked for `TOOLS-04`/`M15`.
- [x] **TOOLS-04** Define the conformance harness and A/B/C/D evaluation fixtures first.
  **Done 2026-08-11.** The shared harness (`agent-conformance`) is closed —
  schema contract, output envelope (TOOLS-03 caps), structured error
  envelope, path confinement, surface/lifecycle rules; the builtin catalog
  passes clean (`tests/builtin.rs`). The A/B/C/D fixtures are closed
  (`agent-eval/src/workload.rs`, listed by `agent-eval --fixtures`): the
  four tool-surface arms (shell-only / current builtin / minimal structured
  ACI with `patch.apply`+`process.run`+`task.complete` / C plus on-demand
  capability loading) and four deterministic coding fixtures with seed
  workspaces and hidden file-content verification
  (`fix_off_by_one`, `implement_stub`, `rename_symbol`, `add_test`),
  self-checked by unit tests and the `--fixtures` input health check. The
  cancellation/timeout/effect-fence checks for *loadable external
  capabilities* remain part of the capability-admission gate (`MOD-05`/
  `ECO-05`), not an open item of this definition.

### Gate 2: close correctness/security blockers

- [~] **CORE-GATE-01** Close `CORE-01`, `CORE-04`, `CORE-06`, `CORE-07`,
  `CORE-08`, `CORE-09`, and `CORE-10` from
  `AUDIT_TODO.md`; external process capabilities remain disabled by default
  until the actual effect/sandbox path passes adversarial tests.
  **Five dependencies and CORE-01's protocol-effect slice are closed.** The
  output broker, cancellation, confined directory-handle operations, standing
  grants, round-surface planning and wire effect fence are landed. CORE-01
  remains open for M13's direct-OS read/network and cross-platform
  confinement residual; CORE-10's current-wire framing, MCP ownership/
  cancellation and broker-value containment are closed (`PLAT-00`,
  2026-08-13); artifact locators are owner/digest identity strings.
  Parse-time decoded JSON DOM budgets, RFC 8785 JCS, explicit
  `legacy.invoke-output.v1` negotiation and the shared adapter fault matrix
  landed 2026-08-14 (`PLAT-04`). Adapter envelope migration remains `PLAT-07`.
  External capabilities
  still enter `Disabled` and stay off the surface until explicit enable;
  autonomous admission remains gated on M13/M15 and `PLAT-00..04`.
- [x] **MOD-03** Separate trusted composition registration from dynamic capability
  registration in names, docs, and authority checks.
  **Verified 2026-08-11.** Names: `ServiceRegistry::register` (typed service,
  "extend the trusted core plane"), `ModuleHost::add_module` (trusted module,
  refused after the host started) and `ModuleHost::register_capability`
  (dynamic capability, "not gated on the host lifecycle: the LLM or any
  external actor can register new capabilities while the runtime is running")
  are distinct entry points. Docs: the code comments name the trusted
  composition plane versus the dynamic capability plane, and the
  "Composition adapters are not ordinary plugins" section below records the
  rule. Authority: a trusted module is registered before start and never
  replaced mid-run; a dynamic capability is admitted by the registry, which
  pins every out-of-process registration to `Experimental` + `Disabled`
  regardless of declaration. What remains open is the *ownership* split —
  admission/grants/activation/quarantine authority moving into a Core-owned
  registry while the runtime keeps catalog views and load/unload scheduling
  (`MOD-05`, part of the `MOD-04` authority slice) — not the registration
  separation itself.
- [~] **MOD-04** Isolate the first real authority slice inside the existing
  kernel crate (effect, approval/policy, output/resource broker, durable
  audit) without moving `RuntimeActor` or creating a second orchestrator.
  **First slice landed 2026-08-11** — `agent-core/src/authority.rs` puts
  each authority behind one named seam of the `CoreAuthority` facade:
  `EventAuthority` (envelope identity/sequence/timestamp, journal append,
  the `emit_durable` durability barrier that broadcasts nothing on a failed
  flush), `ApprovalAuthority` (normalizes the gate's outcome to
  `ApprovalVerdict::{Allowed, Denied, Failed}`, so callers match a verdict
  instead of re-interpreting `AgentResult` + a boolean),
  `EffectAuthority` (every staged effect of every path — builtin,
  capability, wire broker — commits or rolls back through this one seam;
  Runtime checks its mirror and Core now independently rejects a stale
  authority epoch) and
  `OutputAuthority` (the only path from producer output to a model-facing
  `ToolOutput`; absent a broker it passes through and the runtime last-line
  guard remains the backstop). The actor's two effect call sites route
  through `kernel.effect()`. Behavior-preserving: kernel + runtime + full
  workspace tests stay green, plus six new authority unit tests (monotonic
  sequences, durable barrier broadcast, failed-barrier silence, verdict
  normalization, commit/rollback classification, broker pass-through).
  The shadow-mode `AuthorityGate` step of the compatibility order is
  landed as its own slice: `IntentShadowGate`/`ShadowVerdict`
  (agent-contracts), the `CoreAuthorityConfig.shadow_gate` injection and the
  bounded `RuntimeEvent::ShadowDecision` published for allowed and denied
  calls alike, with the standing-grant gate's shadow verdict reusing the
  same matching logic as its legacy `authorize` so the hard invariant
  (shadow `Granted` implies legacy `Allow`) holds by construction and is
  pinned by tests. The `EffectReceipt` step is landed too: `Effect::commit`
  returns the serializable `EffectReceipt` (NotApplied / Applied with
  Durable | DurabilityFailed + evidence / Unknown), the workspace mutation
  emits its transaction id as evidence, the composite aggregates evidence
  and stops at the first non-durable receipt, and the actor keeps the exact
  model-facing semantics (DurabilityFailed still means "was applied but
  the journal failed — recovery required"; Unknown is the never-blindly-
  retry branch). The `AuthorityLease` step (commit-time resource
  enforcement from the `EffectIntent` — the M14 residual) is landed too:
  `AuthorityLease` (agent-contracts §6 of the draft) is minted by
  `execute_tool` for every side-effecting call after approval — carrying
  the operation generation, the derived intent, the covering grant (when
  the shadow gate granted it) and a bounded TTL (`CoreAuthorityConfig::lease_ttl_ms`, default 120 s) — travels with the operation, and is
  validated again at commit time (`valid_at`: generation match + not
  expired). A refused lease rolls the staged effect back and surfaces a
  failed tool result ("the change was not applied: the authorization lease
  expired"), so an operation that overran its authorization window cannot
  mutate the world. `derive_effect_intent` moved to agent-contracts so
  grant matching and lease minting share one normalization; the bounded
  `RuntimeEvent::LeaseIssued` audit row records lease/grant/expiry.
  `MOD-04A` is closed separately below; the only remaining piece of this
  item is compatibility order step 7 (sandboxed shell/process — an
  M13-scoped, OS-level change), whose OS-level *write* filtering slice is
  landed as `MOD-06` and whose Linux TCP deny slice is landed as `MOD-07`.
  Windows Low-IL write confinement is `MOD-08`. Unix `RLIMIT_AS` is
  `MOD-09`. Unix `RLIMIT_FSIZE` is `MOD-10`. Landlock signal scope is
  `MOD-11`. Landlock device-ioctl deny is `MOD-12`. Unix `RLIMIT_NOFILE`
  and inherited-fd close are `MOD-13`. The Windows integrity wrap
  Job-Object commit ceiling is `MOD-14`. Unix `RLIMIT_CORE=0` on sandbox
  `pre_exec` is `MOD-15`. Linux `RLIMIT_NICE`/`RLIMIT_RTPRIO`/`no_new_privs`
  is `MOD-16`. Windows Job `PRIORITY_CLASS=NORMAL` is `MOD-17`.
  UDP/raw/pathname-Unix and
  Windows network fences remain. Inherited TTY detach, `HANDLE_LIST`, and
  Job UI restrictions stay skipped.
  See `docs/ACI_CONTRACT_DRAFT.md` §6 and §7 steps 4-5.
- [x] **MOD-04A** Move context/model/tool/config scheduling behind
  `agent-runtime::RuntimeServices`, then perform the mechanical
  `agent-core` rename as a behavior-free change.
  **Done 2026-08-12** — `agent-runtime::RuntimeServices` is the composition
  seam: `from_registry` resolves the context engine, model transport, tool
  dispatcher, approval gate and event journal from the module host, the
  kernel (`CoreAuthority`) is derived inside the seam (one Arc per run,
  shared with the spawn handle), and all scheduling — configuration,
  model calls, context maintenance/focus transactions, tool lifecycle and
  surface scheduling — moved out of the kernel onto the services. The
  kernel keeps only authority (events, approval, effects, output,
  start/stop) plus the tool-execution wiring (`execute_tool`), query
  rendering with output-authority bounds (`resolve_engine_query`) and the
  authority transactions (acknowledge/restore). The crate is renamed
  `agent-core` with `CoreAuthority`/`CoreAuthorityConfig` types; runtime
  actor routes every scheduling trigger through `services`, keeps the
  authority facade on `kernel`. See `docs/TOOL_ECOSYSTEM_TODO.md`
  "Incremental Core migration" slices 3-4.
- [x] **MOD-05** Split capability ownership: Core owns admission, grants,
  activation/quarantine, and maturity authority; Runtime owns catalog views,
  load/unload scheduling, active state, and per-round surface snapshots.
  **Slice 1 landed 2026-08-12** — admission is now a core decision:
  `agent-core::capability_admission` owns the registration caps
  (`MAX_TOOLS_PER_CAPABILITY` etc.), `validate_static` (id shape, tool
  schema shape/size, manifest-authority derivation, all lock-free) and the
  collision pass `validate_collisions` (duplicate id, missing requires,
  reserved/owned tool names) driven by a registry-built `AdmissionContext`,
  plus the `initial_status`/`initial_activation` decisions (external ->
  Experimental + Disabled). The runtime registry delegates to it with
  identical error messages and check order; behavior is unchanged
  (existing capability registration/surface tests still pass).
  **Slice 2 landed 2026-08-12** — activation/quarantine/maturity state
  ownership moved into a core authority seam:
  `agent-core::capability_state` (`CapabilityStateAuthority` +
  `CapabilityState`) is now the single source of truth for a registered
  capability's effective maturity and activation. The runtime registry
  keeps only the mutable surface mechanics (loaded tools, active marks,
  run lifecycle) that react to the state: every read (`status`/
  `activation`/catalog/surface gates) and every transition
  (`set_activation`/enable/disable/quarantine) routes through the
  authority; checkpoint `snapshot`/`restore` round-trips the state through
  it too. Readers pre-fetch the authority's state map instead of nesting
  locks, and all surface writers stay serialized by the registry's
  `surface_gate`, so the split adds no lock-order hazard. 7 new core unit
  tests pin the authority's contract; the registry's public API and error
  messages are unchanged.
  **Slice 3 landed 2026-08-12** — the effective permission **grant** is a
  core record too: `CapabilityStateAuthority::register` captures the
  admission-validated manifest permissions (`granted_permissions`), and the
  unified dispatcher builds every `CapabilityInvocationContext` from that
  registered grant — never from the live capability object. A capability
  that returns a different manifest after registration cannot escalate what
  it holds; the runtime-directive gate (`runtime:context-control`) also
  checks the registered grant. A new runtime test proves the escalation is
  refused (`invocation_uses_the_registered_grant_not_the_live_manifest`);
  the registry's public API, error messages and honest-capability behavior
  are unchanged. MOD-05 complete: admission, grants,
  activation/quarantine/maturity authority all live in `agent-core`; the
  runtime registry owns catalog views, load/unload scheduling, active
  state and per-round surface snapshots.
- [x] **MOD-06** Land the OS-level write-confine slice of compatibility
  order step 7 (sandboxed shell/process): Linux landlock write fencing in
  `agent-process`, wired for process capabilities and stdio MCP servers.
  **Done 2026-08-12.** `agent-process::landlock` applies a kernel-enforced
  fence in the child right before `exec` (via `pre_exec`): the handled
  access set is tried newest-first across landlock ABIs (v3/v2/v1, using
  the smallest legal struct sizes), each configured write root is opened
  as an `O_PATH` fd in the parent (raw fds — the child's `pre_exec`
  closure only makes syscalls, no allocation), and
  `landlock_restrict_self` — which demands the no_new_privs bit or
  CAP_SYS_ADMIN — runs after `prctl(PR_SET_NO_NEW_PRIVS)`, so the fence is
  irrevocable, inherited by every descendant, and also stops setuid/
  setgid escalation at exec. Reads are deliberately unhandled (the
  executable and loader must stay readable; reads remain gated by the
  app-level broker), so landlock closes the write/destroy/exfil-by-write
  half of the direct-OS gap; TCP deny is `MOD-07`. Wiring:
  `ProcessSandbox.landlock_write_roots` (Linux only); `ProcessHost::
  connect` opens the roots and attaches the closure, degrading to a
  warning on kernels without landlock and failing the spawn (never running
  unconfined) when a configured root cannot be opened or the child cannot
  be confined; `ProcessCapabilityAdapter::from_manifest` confines the
  capability child to its private dir; `McpClient::connect_stdio` confines
  stdio MCP server children to their private temp cwd. Verified on a real
  Linux kernel (WSL2, 6.6): `crates/agent-process/tests/landlock.rs`
  drives the `sandbox_probe` bin under confinement and asserts the probe
  writes inside its root, is refused outside at the OS layer, and still
  reads system files, plus a `ProcessHost` handshake test under the same
  fence. Unit tests cover `O_PATH` root opening and fd-close-on-drop.
- [x] **MOD-07** Land the OS-level TCP-deny slice of the M13 residual:
  Linux landlock ABI v4 bind/connect in the same `agent-process::landlock`
  ruleset as the write fence. **Done 2026-08-20.** When write roots are
  configured, `create_handled_ruleset` tries newest-first: ABI v6
  (TCP + abstract-Unix + signal scope), ABI v4 (TCP), then filesystem-only.
  `EINVAL`/`E2BIG` continue to the next candidate. No
  `LANDLOCK_RULE_NET_PORT` rules are added, so handled TCP is deny-all.
  `tcp_deny_available()` probes ABI v4 for tests. Older landlock kernels
  keep the write fence. UDP, raw, netlink and pathname Unix stay
  unhandled; Windows has no Landlock. Same wiring as MOD-06 (process
  capabilities and stdio MCP). Verified by `sandbox_probe` printing
  `tcp-connect:ok` on `PermissionDenied` and
  `child_cannot_connect_tcp_under_landlock` (skipped when ABI < 4). Do
  not mark M13 closed.
- [x] **MOD-08** Land the Windows OS-level write-confine slice of the M13
  residual: Low Integrity Level fencing in `agent-process::integrity`.
  **Done 2026-08-20.** When `integrity_write_roots` is non-empty, the parent
  labels each root Low and re-spawns through this same executable with
  `__FOCUS_AGENT_INTEGRITY_WRAP_V1__`. A CRT constructor hijacks that
  child, drops to Low IL, then CreateProcess-es the real program with
  inherited stdio/env/cwd. Low IL cannot write up to Medium objects.
  Reads and TCP stay unhandled. The wrap holds `KILL_ON_JOB_CLOSE` plus
  a 512 MiB `PROCESS_MEMORY` ceiling (`MOD-14`) so TerminateProcess of
  the wrap still kills the real child and a Low-IL allocator cannot
  exhaust the machine. A missing
  target program fails as a spawn error before wrap. Wired for process
  capabilities and stdio MCP servers. Verified on Windows:
  `crates/agent-process/tests/integrity.rs` drives `sandbox_probe` through
  the wrap and asserts writes inside the root, OS-level refusal outside,
  and a system-file read, plus a `ProcessHost` handshake under the same
  fence. AppContainer stays out of v0. Do not mark M13 closed.
- [x] **MOD-09** Land the Unix address-space ceiling that pairs with
  Windows Job-Object process memory. **Done 2026-08-20.**
  `ProcessSandbox.max_memory_bytes` maps to `RLIMIT_AS` and is applied
  in the same `pre_exec` as landlock so a last-hook-wins toolchain cannot
  drop rlimits when write roots are set. The capability adapter defaults
  to 2 GiB VAS (coarser than the 512 MiB Windows commit charge; 512 MiB
  AS often fails ordinary exec of debug binaries). Stdio MCP servers get
  the same memory cap; CPU/nproc stay unset on MCP because `RLIMIT_NPROC`
  is per-user. Verified by `crates/agent-process/tests/rlimits.rs`
  (`child_cannot_allocate_past_rlimit_as`, handshake under a
  production-sized ceiling). File-size ceilings are `MOD-10`. Do not mark
  M13 closed.
- [x] **MOD-10** Land the Unix per-file size ceiling. **Done 2026-08-20.**
  `ProcessSandbox.max_file_bytes` maps to `RLIMIT_FSIZE` in the same
  `pre_exec` as landlock and `RLIMIT_AS`. The capability adapter and
  stdio MCP servers default to 256 MiB. This is a per-file `EFBIG`/
  `SIGXFSZ` cap, not I/O bandwidth and not a Windows Job-Object feature.
  Verified by `child_cannot_write_past_rlimit_fsize` and a handshake
  under a production-sized ceiling. I/O bandwidth quotas remain. Do not
  mark M13 closed.
- [x] **MOD-11** Land Landlock ABI v6 signal scoping. **Done 2026-08-20.**
  The v6 `scoped` field already denied abstract Unix; it now also sets
  `LANDLOCK_SCOPE_SIGNAL` so a confined child cannot `kill` processes
  outside its domain. Signalling itself still works; parent→child tree
  kill on cancel is unaffected. `signal_scope_available()` probes ABI v6
  for tests. Older landlock kernels keep the write fence (and TCP deny
  on ABI v4). Verified by `sandbox_probe signal <pid>` and
  `child_cannot_signal_outside_its_landlock_domain` (skipped when ABI < 6).
  Do not mark M13 closed.
- [x] **MOD-12** Land Landlock ABI v5 device-ioctl deny. **Done 2026-08-20.**
  `LANDLOCK_ACCESS_FS_IOCTL_DEV` is included in the newest handled-access
  candidate but never granted on write roots, so newly opened
  character/block devices cannot ioctl. Inherited stdio is unaffected.
  `ioctl_dev_deny_available()` probes ABI v5 for tests. Older landlock
  kernels keep the write fence. Verified by `sandbox_probe` printing
  `ioctl-dev:ok` on `PermissionDenied` and
  `child_cannot_ioctl_devices_under_landlock` (skipped when ABI < 5).
  Do not mark M13 closed.
- [x] **MOD-13** Land the Unix open-file ceiling and inherited-fd close.
  **Done 2026-08-21.** `ProcessSandbox.max_open_files` maps to
  `RLIMIT_NOFILE` in the same `pre_exec` as landlock, `RLIMIT_AS` and
  `RLIMIT_FSIZE`. After landlock, the child closes inherited fds other
  than stdin/stdout/stderr (fixed 4096-fd scan, `close` only) so a
  parent descriptor without `O_CLOEXEC` cannot leak across exec. The
  capability adapter and stdio MCP servers default to 1024 fds. Not I/O
  bandwidth. Verified by `child_cannot_open_past_rlimit_nofile`,
  `child_cannot_keep_inherited_fds_without_cloexec`, and a handshake
  under a production-sized ceiling. I/O bandwidth quotas remain. Do not
  mark M13 closed.
- [x] **MOD-14** Land the Windows integrity-wrap Job-Object commit ceiling.
  **Done 2026-08-21.** The wrap's job was `KILL_ON_JOB_CLOSE` only, so the
  Low-IL program it CreateProcess-es (including stdio MCP children) had
  no OS memory cap — ProcessHost's job covers the wrap process, not the
  real child. The wrap job now sets `JOB_OBJECT_LIMIT_PROCESS_MEMORY` to
  512 MiB (capability Windows default) and
  `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION`. ProcessHost sandbox jobs
  also set the unhandled-exception flag. Not I/O bandwidth and not
  AppContainer. Verified by `sandbox_probe jobmem` and
  `wrapped_child_cannot_commit_past_job_process_memory` (skipped when an
  outer job prevents assign). Do not mark M13 closed.
- [x] **MOD-15** Land Unix core-dump disable on sandbox `pre_exec`.
  **Done 2026-08-21.** `apply_unix_rlimits` always `setrlimit(RLIMIT_CORE,
  0)` when it runs (capability and stdio MCP children already take this
  path). Other rlimit arguments keep `0` = unlimited; there is no
  `max_core_bytes` field, because that `0` would be ambiguous. Probe via
  `getrlimit` (`sandbox_probe core`), not by crashing the child. Verified
  by `child_sees_rlimit_core_zero_when_sandbox_pre_exec_runs` (Unix;
  skipped on this Windows host). Not I/O bandwidth. Do not mark M13
  closed.
- [x] **MOD-16** Land Linux priority freeze on sandbox `pre_exec`.
  **Done 2026-08-21.** `apply_unix_rlimits` always `setrlimit(RLIMIT_NICE,
  0)` and `setrlimit(RLIMIT_RTPRIO, 0)` on Linux, then
  `syscall(SYS_prctl, PR_SET_NO_NEW_PRIVS)` so a parent with a raised
  nice/rtprio ceiling cannot leak into the child and a setuid exec cannot
  escalate even when landlock is skipped. Not fields (same always-zero
  meaning as CORE). Probe via `getrlimit` / `PR_GET_NO_NEW_PRIVS`
  (`sandbox_probe pri`). Verified by
  `child_sees_priority_rlimits_and_no_new_privs_when_sandbox_pre_exec_runs`
  (Linux; skipped on this Windows host). Do not mark M13 closed.
- [x] **MOD-17** Land Windows Job-Object NORMAL priority pin.
  **Done 2026-08-21.** ProcessHost sandbox jobs and the integrity wrap job
  set `JOB_OBJECT_LIMIT_PRIORITY_CLASS` to `NORMAL_PRIORITY_CLASS` so the
  child cannot raise HIGH/REALTIME. `BREAKAWAY_OK` /
  `SILENT_BREAKAWAY_OK` stay unset (default-deny). Not a rate limit, not
  UI, and not `HANDLE_LIST`. Verified by
  `created_job_pins_normal_priority_and_denies_breakaway` and
  `wrapped_child_sees_the_wrap_job_normal_priority_class` (`sandbox_probe
  jobprio`; skipped when an outer job prevents assign). Do not mark M13
  closed. After MOD-17 there is no further allowed v0 sandbox slice;
  do not invent `MOD-18` from UDP/raw/pathname-Unix, Linux absolute
  reads, Windows OS-level network, I/O quotas, multiplexing, or Named
  Pipe/UDS.
- [x] **COMPOSE-01** Extract reusable application/bootstrap composition from
  `agent-tui` for TUI/CLI/eval while keeping it stateless and actor-free.
  **Done 2026-08-12.** New composition-root crate `agent-compose`: one
  stateless, actor-free `compose(ComposeConfig)` async function wires the
  module host (context/model/tool/approval + optional event/artifact
  modules), the `CapabilityAwareDispatcher` (when `capability_aware`),
  the kernel services from the typed registry, and spawns the
  `RuntimeInstance` — without starting it, so the caller can subscribe to
  events first (preserving `RunStarted` visibility). The config carries
  the workspace, engine, model, approval gate, base tool dispatcher,
  capability-aware flag, journal, artifact store and output broker; the
  crate also owns the shared `ContextPolicy` parsing +
  `build_context_engine` (append/rolling/dynamic/service, store always
  under the run's state dir) and the env-driven `model_from_env`
  (OpenAI-compatible provider or the moved `MockModelTransport`).
  `agent-tui` and `agent-eval` now both call it: the TUI passes the full
  interactive wiring (capability-aware dispatcher, journal, artifacts,
  output broker), the eval harness passes its scripted engine/model/
  allow-all approval with a plain dispatcher and no journal. Behavior
  preserved — agent-eval's fixture tests (real tool surface end to end)
  and the full workspace suite stay green.
- [x] **ECO-02** Make manifest identity/path/source validation and process stdout/stderr
  accounting non-bypassable.
  **Verified 2026-08-11.** Identity: `validate_capability_id` runs at registration and
  again in `ProcessCapabilityAdapter::from_manifest` (a manifest the registry never saw
  is refused too — `from_manifest_rejects_ids_that_could_escape_a_path`). Path: the
  capability working directory is `temp_dir()/context-agent-capability-<id>-<uuid>`,
  unpredictable and path-safe. Source: source is the transport, not a self-declared
  field — `CapabilityTransport::Builtin` (host-registered, pre-start, keeps its declared
  maturity and enters `Enabled`) versus any out-of-process transport (pinned to
  `Experimental`, enters `Disabled`, explicit enable only;
  `external_capabilities_start_experimental_regardless_of_declared_status`). Process
  stdout/stderr accounting: the child's stderr is piped into a bounded 64 KiB tail
  (`stderr_capture_bytes`), stdout crosses the wire through the frame bound and the
  kernel-level output broker caps `model_content` at 16 K chars with artifact spill —
  there is no path from a capability's output to the model that bypasses the broker.

### Gate 3: complete and test the minimal ACI

- [x] **TOOLS-05** Add patch-set/file-revision semantics; evaluate against `edit.replace`
  before deprecating anything. `edit.patch` lands the file-revision
  semantics: `fs.read` reports the content revision (SHA-256 hex), hunks
  are exact-match and require unique model-visible anchors; legacy ordinal
  occurrence remains parser-only compatibility. Nothing is deprecated, and
  an optional `base_revision` refuses an edit based on stale
  content, and multiple files commit as one composite effect with
  journal-backed rollback evidence. `edit.replace` remains the simpler
  single-hunk tool; no deprecation.
- [x] **TOOLS-06** Add structured process/session semantics and process-tree cleanup;
  retain shell as a controlled fallback. `process.run` executes an explicit
  argv (no shell) with workspace-relative cwd, explicit env overrides,
  bounded tail + artifact output, and whole-tree kill on timeout/cancel;
  `process.session` adds the start/poll/stop protocol for long-running
  processes (shared per-dispatcher session registry, drained output,
  tree-killing stop). `shell.exec` stays as the raw-string fallback.
- [x] **TOOLS-07** Add artifact range fetch and consistent result paging.
  `artifact.read` is the read side of the `artifact://` contract: it
  resolves a reference (confined to `.focus-agent/artifacts/`, refusing
  foreign schemes, query/fragment components and traversal), returns a
  bounded numbered line range with `has_more`/`next_start_line` paging
  metadata, and reads with a hard byte cap (append-only artifacts cannot
  grow past the bound between a size probe and the read). `fs.list` and
  `search.grep` gain snapshot-backed cursor paging: an overflowing result
  spills its full sorted listing to an artifact (as before) and the result
  now carries an opaque `cursor` (`<artifact_ref>#<offset>`); a later call
  with that cursor serves the next page from the *same immutable snapshot*
  instead of a fresh scan, so directory/file changes between pages cannot
  cause duplicates or gaps. Cursors past the snapshot end and malformed
  cursors are clean errors, and `artifact.read` can still page through any
  spill directly.
  Follow-up live evidence on 2026-08-24 showed that publishing those optional
  opaque cursors on every model call induced fabricated artifact identities.
  The model-visible surface is now consolidated on `artifact.read`; per-tool
  cursor parsing remains only for compatibility with trusted non-model
  callers.
- [x] **TOOLS-08P** Validate the TaskToolRequirements/round-surface first
  slice: bounded TaskRecord CAS, Must/Prefer/KeepReady packing and degradation,
  lifecycle refresh, bounded decision events, one final snapshot, runtime
  surface revision, and the requirement slice now carried by RuntimeCheckpoint
  v3. This historical item did not include the
  complete TaskAnchor or completion transaction.
- [x] **TOOLS-08** Add TaskAnchor-driven tool roots and structured completion controls with the
  `CTX-10` CompletionRecord transaction and GC root transfer. The active
  task's tool-demand set is the tool-lifecycle GC root set: the runtime
  passes the task's requirement names to `gc()` at the per-round safe
  point, so a tool the task requires (Must/Prefer/KeepReady alike) is
  never aged off the surface by idle GC — roots protect the silent idle
  path; an explicit unload still works and surface planning fails closed
  on Must demand. Completion is now a structured model control:
  `task.complete` packages a bounded summary plus bounded `artifact://`
  refs as a typed `RuntimeDirective::CompleteTask`; the runtime validates
  it (same caps as a persisted `CompletionRecord`) and commits it at the
  turn's safe point — after the turn commits, never mid-operation —
  through the same CTX-10 transaction as `/done` (prepare, engine
  transition, task flip, `TaskCompleted`, post-completion GC). No active
  task at the safe point drops the proposal with a warning; a committed
  turn is never undone by a completion failure.
- [x] **TOOLS-08A** Split capability process state from per-tool surface
  state; loading one tool must not expose all sibling schemas, and external
  tools must receive the same root/idle cooling semantics as builtins. The
  capability registry keeps per-tool surface state (`tool_states`: each
  loaded tool's lifecycle + last-used tick) separate from the capability's
  process lifecycle (`run_state`), so loading one tool surfaces exactly
  that tool — siblings stay `Available` — and each loaded tool ages
  independently. The unified `gc()` safe point now ages the capability
  registry with the same idle thresholds as the builtin catalog
  (Loaded → Warm → Unloaded) and honors the same TaskAnchor roots: a
  task-required capability tool is never cooled by idle GC, and executing
  a tool refreshes its idle clock. Explicit unload still works (roots
  protect only the silent idle path).
- [x] **TOOLS-08B** The first slice carries separate catalog, task-requirement
  and focus revisions, hard Must/Prefer/KeepReady degradation, atomic builtin
  capture, capability surface-gate serialization, and a composite common-cut
  protocol verified under concurrent mutation. Complete TaskAnchor,
  Episode/Focus and execution-policy sources remain owned by `TOOLS-08`.
- [x] **TOOLS-08C** Bounded round-surface diagnostics, monotonic revisions and
  `ModelStarted` ordering are verified; per-row demand provenance and the
  live-restore rebase audit event are landed. The per-row demand provenance
  is `ToolSurfaceOrigin` (`DispatcherRequired` / `TaskRequirement` /
  `CatalogLoadedOptional`, legacy rows deserialize as `Unknown`), carried on
  selected and omitted rows alike and preserved by budget omission. The
  bounded live-restore rebase audit is `RuntimeEvent::RuntimeRestored`
  (checkpoint version, restored/current run ids, focus/surface revisions,
  rebased requirement count, whether capability state applied), emitted as a
  mandatory barrier whose append failure fences mutation as
  recovery-required (`CORE-03`).
- [x] **TOOLS-09** Evaluate local symbol/diagnostic navigation as optional
  first-party tools; do not add embeddings/vector storage.
  **Evaluation.** The coding loop needs two navigation primitives beyond
  regex search: *symbol navigation* (where is a definition) and *diagnostic
  navigation* (jump from a `file:line:col` error to its source context).
  `search.grep` finds text occurrences but does not distinguish definitions
  from references or know a language's declaration shapes, and a model with
  raw compiler output must parse positions itself and guess at files. Three
  candidate approaches were weighed: (a) vector/embedding retrieval — ruled
  out by invariant 8 (v0 stays non-vector) and, more importantly, wrong for
  this job, since symbol/diagnostic navigation is exact-text and structural,
  not semantic; (b) a language-server protocol adapter — precise but
  heavyweight: one process per language, large schema/maintenance/attack
  surface, belongs in the Ring-3 extension plane, not the default first-party
  set; (c) pure-local lexical scanning with per-language declaration rules,
  bounded output, artifact spill and workspace confinement — cheap,
  deterministic, dependency-free, and covers the common cases. (c) was
  chosen. **Landed:** `code.symbols` (language-aware definition scan for
  Rust/Python/TS/JS/Go/C/C++/Java/C#/Kotlin/Scala/Swift/Zig; comments,
  ignored dirs and inline comments are skipped; C-like function detection is
  an explicit heuristic; results are `file:line:col  kind name` rows with
  snapshot-backed cursor paging) and `code.diagnostics` (parses diagnostic
  text — rustc `-->` style included — deduplicates `file:line:col`
  positions, resolves each as a workspace-relative confined path and shows
  the surrounding source lines; escaping or missing paths are reported
  unresolved, never followed; output is hard-bounded by a char budget). Both
  are catalog-optional: loaded on demand through `capability.manage`, never
  always-on, and they obey the same envelope, confinement and spill rules as
  every tool. No embeddings, no vector storage, no index.
- [x] **TOOL-ENV-01** Publish the bounded Runtime Facts profile and make the
  selected `shell.exec` dialect/version agree exactly with its schema.
- [x] **TOOL-EDIT-01** Add revision-aware `edit.replace` refusal and bounded
  stale/no-match diagnostics; retain exact, staged mutation semantics.
- [x] **TOOL-VIEW-01** Unify the model-visible workspace view and remove raw
  runtime/VCS internals from ordinary file navigation.
- [x] **TOOL-ERROR-01** Add trusted bounded failure classes/recovery hints and
  evaluation attribution without blind retries or cost exclusion.
  **Follow-up 2026-08-17:** Core strips reserved producer keys and writes
  `metadata._runtime` plus a model-visible `runtime_failure:` header.
  `MissingProjectMarker` is evidence-gated (command + subcommand + output
  evidence + true absence). Failed tool results stay on the TurnFrame and
  do not heat C via `WorkingSetSignal`.

These four items are the measured tool-reliability preflight for the next
Context Bench wave. They do not reopen completed `TOOLS-05..09` safety work
and do not replace M12/M13 sandbox closure.

### Gate 3.5: Platform protocol, discovery and managed workers

Complete tasks in the order shown. `PLAT-00..04` are the immediate boundary
and reliability gate; changing local transport before them is explicitly out
of order.

- [x] **PLAT-00 — P0a: contain the current wire boundaries before redesign.**
  Fix the existing codecs and ownership now: hard-cap host→child requests,
  broker answers, frames and exchange totals; cap known decoded large fields;
  reject oversize while
  reading in ProcessHost, context-service and MCP; split frames independently
  of OS read chunking and treat EOF fragments correctly; retain/kill/poison
  MCP children on cancel/timeout;
  bound notification/stderr floods; canonicalize returned call/tool identity;
  and prevent a valid current `ProcessInvokeResponse` (including an empty
  effect list) from falling through to the legacy decoder. Large
  file/result bodies become range or artifact refs. This may land in parallel
  with PLAT-01 and must not wait for a new transport.
  **2026-08-13:** the wire boundaries are contained. One shared bounded frame
  codec (`agent-process::frame`) caps outbound frames before a byte is written
  and enforces the in-flight bound incrementally on read; ProcessHost,
  context-service and MCP all fail closed on oversize/partial/malformed/
  version/id/envelope faults and poison + kill owned child trees. The codec
  preserves multiple frames delivered by one OS read; session state and
  unpredictable request ids reject pre-sent/stale logical responses.
  `ProcessHost` adds a cumulative per-call byte budget and a control-plane
  answer cap; the MCP client owns its server child (kill-on-cancel/timeout/
  poison, reap, `stop()` teardown, poisoned-client replacement, notification-
  flood cap) and surfaces `Cancelled` on cancellation; the broker serves large
  `fs.read` results through an allocation-bounded workspace read as a 256 KiB
  prefix with `byte_len`/`truncated` metadata, never full-read-then-truncate.
  Process effects have count/path/per-effect/aggregate decoded-byte caps before
  base64 decode and staging. Current and legacy process-
  capability responses canonicalize `call_id`/`tool_name` from the request,
  and an empty-effects current envelope no longer falls into shape-guessing.
  Regressions cover outbound oversize, cumulative/broker flood, same-write
  frames plus stale-id rejection, partial EOF, oversized service requests,
  notification flood and cancel-after-spawn. A follow-up containment slice
  fails closed on process `WireEffect`s until actual-intent proof exists,
  reports partial composites truthfully with a recovery fence, bounds local
  process output/artifact capture, and binds artifact reads/broker/completion
  refs to the current run. **PLAT-04 landed 2026-08-14:** RFC 8785 JCS
  argument digests, explicit `legacy.invoke-output.v1` handshake (plain
  `ToolOutput` default-deny), parse-time JSON DOM budgets, artifact
  owner/digest locators, and the shared process/context/MCP fault matrix
  in `agent-conformance`. Adapter envelope migration remains `PLAT-07`.
  General artifact-range transport for very large bodies remains later work.

- [x] **PLAT-01 — P0: freeze the Agent OS boundary.** The narrow in-process
  `CorePort` is landed. `RuntimeServices` retains only the trait object as its
  Core facade while its fields/scheduling methods, concrete `RuntimeActor`,
  concrete Core and event/approval/effect/output handles are private;
  capability/plugin admission/state remain explicit Core primitives. Public constructors and
  `spawn_runtime` remain trusted composition seams. Effect commit/rollback
  requests carry run/turn/operation/generation/lease identity and authority
  rejection is typed. `agent-conformance` checks forbidden production paths,
  bottom-layer contracts and concrete-implementation composition roots.
  Commit authority rejection is typed; rollback remains best-effort cleanup
  with identity errors surfaced. This slice deliberately does not claim an
  unbypassable security boundary. At PLAT-01 landing, V1 trusted same-process
   Runtime and Core lacked both an independent current epoch and a recoverable
   operation ledger. PLAT-03a1-a4 have since closed the epoch, bounded
   identity/state, journal-first persistence, builtin workspace reconciliation
   and authority-prefix checkpoint validation; unmanaged effects and the
   isolation threat model remain.
- [x] **PLAT-02 — P0: specify the transport-independent envelope.** The new
  bottom-layer `agent-platform-protocol` crate separates
  `message_id`, physical `request_id`, model `call_id`, logical
  `operation_id`, attempt and `effect_id`; carry run/task/turn/scope,
  remaining deadline, version/features/schema digest and bounded causality.
  It adds exact negotiated profiles, strict typed/non-nil IDs, explicit
  success/error responses, structured protocol/domain errors with validated
  retry/effect-state dispositions, monotonic remaining deadlines, bounded
  one-hop causality and stateless validators. Ping/pong is liveness only,
  never authority or session state. Core envelope fields fail closed on
  unknown input. Digest helpers intentionally hash caller-supplied semantic
  bytes; RFC 8785 JCS and explicit legacy negotiation landed `PLAT-04`;
  adapter envelope migration remains `PLAT-07`; external operation
  routing/admission and
  unmanaged-effect semantics remain `PLAT-03`, while the Core-local ledger,
  query and workspace reconciliation are landed.
- [~] **PLAT-03 — P0: make retry and recovery honest.** **a1-a4 landed
  2026-08-13:** Core owns an `AtomicU64` authority epoch, recovered and advanced
  by the production journal;
  Runtime requests compare-and-swap advances and keeps only a mirror. Core
  rejects stale tool dispatch before and after approval and independently
  rejects stale effect commit. Cancellation advances the Core fence before
  any await or cleanup and records an exact-identity terminal reservation, so
  cancellation that wins the admission race still prevents dispatch. Runtime
  remains the only task/turn orchestrator. Core
  now also owns a bounded operation registry keyed by typed
  operation identity + canonical Rust argument digest; it assigns `EffectId`,
  binds leases to operation+digest, refuses conflicting reuse, prevents exact
  duplicate dispatch/commit, never evicts unresolved state, and reports
  found/expired-or-possibly-seen/unseen through its in-process query. Its
  bounded seen filter is fail-closed (false positives refuse work).
  **a3 landed 2026-08-13:** a contracts-only `OperationJournal` is injected by
  composition; Core persists and `sync_all`-barriers epoch/full-operation
  transitions before publishing memory state. Recovery validates bounded,
  checksummed contiguous records, repairs only a structurally incomplete final
  fragment and otherwise fails closed; writes stop before recovery/file limits
  can poison the next startup. Startup durably advances the recovered epoch
  before exposure. Unix synchronizes newly created parent directories; Windows
  synchronizes the journal file but retains an explicit power-loss
  directory-entry limitation.
  **a4 landed 2026-08-13:** Core preallocates a stable `EffectId` before every
  side-effecting dispatch and passes exact operation/digest/effect identity into
  builtin workspace mutations. Their bounded, exclusively locked, checksummed
  journal durably records prepare/commit/rollback evidence. Startup reconciles
  that evidence with current file hashes: proven not-applied/applied states are
  terminalized; partial, corrupt, unmanaged or ambiguous states remain
  unresolved and raise `RecoveryRequired`. Generic shell/process spawn/exit
  recovery is landed; out-of-process capability/MCP invoke recovery is landed.
  **Checkpoint v4 landed 2026-08-13:** checkpoints carry a stable authority
  journal lineage/generation/prefix/digest marker; restore validates it as an
  ancestor before mutation and advances rather than rewinds the live epoch.
  Markerless ephemeral checkpoints are same-run only.
  **Operation control slice landed 2026-08-13:** the protocol crate defines
  bounded query/cancel routes whose success bodies preserve exact Core truth;
  `RuntimeHandle` serializes query and exact-current-tool cancellation through
  `RuntimeActor`. Query remains available behind recovery. Core arbitrates
  cancellation and terminalization under one authority transition: a won
  cancel persists its epoch fence plus `CancelledBeforeCommit` before the
  durable `TurnCancelled` barrier completes; a terminal/commit race that won
  first returns unchanged Core truth, and partial WAL failure fences both
  layers. That cancellation terminal proves only that a
  Core-mediated commit did not start; it cannot undo mutations an approved
  non-transactional child already performed. Requests omit `effect_id` and
  treat the returned Core snapshot as effect truth. This trusted seam performs no
  external transport authentication by itself. The Platform router now calls
  a trusted connection-scoped authorizer, canonicalizes Core truth and
  forwards only through `RuntimeActor`; Core upgrades its one-shot admission
  permit to a distinct dispatch permit only after `OperationAccepted` and
  `ToolStarted` publish successfully.   **WAL compaction landed 2026-08-13:** `FileOperationJournal::compact`
  folds epoch plus current snapshots into a new generation, retains every
  operation identity, stores the pre-compact tip in a bounded ancestor
  list, and fail-closes discarded mid-generation prefixes. Empty journals
  are a no-op; callers cannot append `Compacted`. CorePort exposes
  `compact_authority_journal`. **Process spawn/exit recovery landed 2026-08-13:**
  `shell.exec` / `process.run` / `process.session` persist spawn then wait
  in `.focus-agent/authority/process-effects.jsonl`. Startup maps
  never-spawned process tools to `NotApplied`, a durable wait to
  `CompletedValue` (Executing only), and missing-exit crash windows to
  `Ambiguous`. Leftover trees are signalled only when the OS create-time
  token still matches. `process.session` recovery is keyed by the start
  identity; poll/stop never spawn. This does not roll back child mutations and does not
  cover HTTP/gRPC brokers. **Authenticated operation-control session landed 2026-08-14:**
  trusted composition installs a bounded grant; `AuthenticatedOperationControlAdapter`
  binds it to one connection, stamps `authority_ref` from that session, and
  forwards query/cancel through the existing router. Peer-supplied refs cannot
  escalate, revoked sessions are denied, and oversize/malformed frames never
  enter the actor. Framed JSON-lines operation-control over an inherited-pipe
  analogue landed 2026-08-14: `FramedProtocolSession` reuses the shared
  `read_frame`/`encode_frame_bytes` codec, poisons on inbound framing errors,
  and leaves the connection usable when an outbound encode is rejected before
  any byte is written. The authenticated adapter still consumes one frame
  body and never owns the pipe; local transport identity is not a Core grant.
  Named Pipe/UDS remain PLAT-08.
  **Remote invoke recovery landed 2026-08-14:** effectful capability/MCP
  invokes persist reserved (operation-id idempotency key) then dispatched
  before the child call, then ack. Never-sent work is `NotApplied`, a
  durable Completed/Failed ack is `CompletedValue`, dispatched-without-ack
  is `Ambiguous`, and an in-flight key refuses a second send. `Staged`
  acks without matching workspace evidence stay ambiguous. A future HTTP
  broker must reuse these persist APIs; peer mutations are never rolled
  back. Only
  brokers with a durable idempotency barrier may claim at-most-one commit;
  every other ambiguous crash window returns `OutcomeUnknown`, never blind
  replay. The current persisted authority is not crash exactly-once and cannot
  police a malicious Runtime in the same address space.
- [x] **PLAT-04 — P0: migrate and prove the common contract.** After PLAT-00
  containment and PLAT-02 semantics, bounded argument/result envelopes,
  artifact scheme/run/owner/digest locators (live captures stay drafts until
  seal), parse-time decoded JSON DOM budgets, RFC 8785 JCS
  (`ArgumentDigest::from_json`), and explicit `legacy.invoke-output.v1`
  handshake negotiation are landed. Plain `ToolOutput` is default-deny unless
  that feature is crossed at ping. One conformance/fault matrix over
  process, context and MCP adapters covers duplicate/stale-id, effect-disconnect,
  cancel-late, crash/reconnect, version/schema mismatch,
  malformed/multi-frame/truncated/oversize frames, JSON DOM bombs and
  broker/notification flood (`crates/agent-conformance/tests/adapter_fault_matrix.rs`).
  Stale generation remains Core/Platform (`agent-runtime` tests); isolated
  adapters do not depend on `agent-core`/`agent-runtime`. Adapter envelope
  migration onto Platform DTOs remains `PLAT-07`. This slice does not start
  PLAT-05/06/08 or invent an HTTP broker.
- [x] **PLAT-05 — P1: separate supervision, transport and protocol.** Refactor
  the currently split adapter-owned process control into
  `ProcessSupervisor` and `DuplexTransport` on top of the landed bounded
  `FramedProtocolSession`; preserve current owned-child kill-tree/poison
  behavior and keep inherited anonymous pipes as the first backend. The same
  conformance fixtures must exercise direct and pipe adapters.
  **Slice 1 landed 2026-08-20:** `ProcessHost` composes `ProcessSupervisor`
  (tree kill, then await reap) and `StdioDuplexTransport`
  (`FramedProtocolSession` on inherited pipes). Handshake/timeout/cancel/
  frame-violation paths reap before return, matching the MCP client.
  **Slice 2 landed 2026-08-20:** MCP stdio owns `ProcessSupervisor` instead
  of a raw `Child`; supervisor Drop is the tree-kill backstop; reap clears
  the pid. Native process capabilities already used `ProcessHost`.
  **Slice 3 landed 2026-08-20:** `DuplexTransport` is the byte-duplex seam
  (`recv` / `send_encoded_line` / poison). `FramedProtocolSession`
  implements it; `ProcessHost` protocol helpers take `impl DuplexTransport`
  instead of `ChildStdout`. `StdioDuplexTransport` remains the inherited
  anonymous-pipe backend. Conformance process-direct rows go through the
  trait (`process_direct`); pipe rows stay `ProcessHost`. Named Pipe/UDS
  remain PLAT-08. Do not start PLAT-06 health/epochs in this slice.
  **PLAT-05 supervision/transport split is complete** for the first backend.
- [~] **PLAT-06 — P1: lifecycle and backpressure.** Add peer/host epoch,
  Ready/Degraded/NotServing/Quarantined health, bounded restart/circuit
  breaking, deadline propagation, cancellation acknowledgement and
  coalescible bounded progress. Keep single-inflight until measurement or
  managed-worker workloads justify multiplexing; connection state is never
  task or authority state.
  **Slice 1 landed 2026-08-20:** `agent-process::health` (`ConnectionHealth`,
  `ConnectionEpoch`, `RestartCircuit`; default max 3 replacements; first
  connect is not a restart). `ProcessHost::status` / `epoch`; ping carries
  `host_epoch` and reads peer `epoch`. Stderr capture at cap is Degraded,
  not poison. MCP invoke and process-capability `start()` replace a
  quarantined child only after `try_acquire`; exhaustion keeps the dead
  client/host. Do not mark PLAT-06 done.
  **Slice 2 landed 2026-08-20:** peer cancel-ACK and coalescible progress.
  `ProcessHost` writes `op=cancel` after a written request and waits
  250 ms for `{cancelled: true}`; a silent peer cannot stall past that
  bound. MCP sends `notifications/cancelled` and waits the same bound
  for a discarded matching response. Cancel before write does not poison.
  Kill-then-reap remains settlement. `progress` frames coalesce (drop
  intermediates) and trip a per-call count cap; they are connection
  backpressure, never task or Core authority. Remaining: multiplexing
  (stay single-inflight).
- [ ] **PLAT-07 — P1: publish the Platform SDK and adapters.** Extract stable
  wire DTOs/SDK after the protocol passes conformance; map process/context/
  MCP and future WASI/remote transports into it. A Skill package depends on
  this contract, but only its executable Tool/Hook/worker is a protocol peer.
  MCP interoperability and OS identity never replace Core authority.
- [ ] **PLAT-08 — P2: add local-service transports only after benchmarks.**
  Implement Windows Named Pipe and Unix Domain Socket for persistent,
  independently started or reconnectable services, with restrictive ACLs /
  owner-only socket paths and peer identity checks. Compare CPU, p50/p95
  latency, throughput and peak memory against the anonymous-pipe backend;
  do not migrate Platform-owned one-to-one children without evidence.

Dependency order:

```text
PLAT-00 [done] -> PLAT-01 [done] -> PLAT-02 [done]
   -> PLAT-03 [partial: a1-a4 + workspace/process/remote-invoke recovery + checkpoint v4 + compaction + in-process session auth + framed inherited-pipe analogue done]
   -> PLAT-04 [done]
   -> PLAT-05 [done: supervisor + DuplexTransport + stdio first backend] -> PLAT-06 [slice 1: health/epochs/restart; slice 2: cancel-ACK + progress]
   -> PLAT-07 -> TOOLS-10/11/12 -> AGENT-01/02
   -> deployment need + transport measurements -> PLAT-08
   -> M13 closed + M15 evidence -> AGENT-03
```

- [x] **TOOLS-10** Introduce a bounded, read-only federated resource
  descriptor/ref contract. Prototype Context + Tool discovery behind current
  providers before deciding whether one `runtime.search` model surface should
  replace `context.manage`/`capability.manage` search operations. Depends on
  the current containment plus bounded Platform envelope/identity rules in
  `PLAT-00..04`.
  **Landed 2026-08-14 (Context + Tool prototype).** Shared planner and
  `ResourceDescriptor` live in `agent-contracts`; `capability.manage search`
  indexes name/description/owner/state/risk (case-insensitive, residual scan
  if no token hits) instead of case-sensitive name-contains. Search does not
  load tools and is `TransientNoPersist`. Decision: no public `runtime.search`
  yet. Remaining: Artifact/Task/Agent/Skill/Event providers (`TOOLS-11/12`
  still own lifecycle split and metrics).
- [ ] **TOOLS-11** Split descriptor/schema lifecycle from invocation/effect
  lifecycle. Preserve current catalog cooling behavior, add typed invocation
  states/reasons, and prove an active call cannot be unloaded, cancelled late,
  or committed after its authority generation changed.
- [ ] **TOOLS-12** Extend lifecycle metrics with discovery hit/miss reason,
  schema residency/cold-start cost, invocation wait/run/commit latency,
  cancellation cleanup, host reuse, and per-task root reasons.
- [ ] **AGENT-01** Define bounded `AssignmentCard`, `HandoffCard`, child ref,
  budget/deadline, permission-subset, and event/checkpoint contracts. The main
  TaskAnchor and CompletionRecord remain parent Runtime authority. Specify
  only after `PLAT-00..04`; do not invent a separate child-Agent wire.
- [ ] **AGENT-02** Specify `agent.manage` search/inspect/spawn/send/wait/
  status/cancel/collect and the managed-child lifecycle. Return bounded
  summaries plus artifact/evidence refs; never concatenate child transcripts
  into the parent prompt.
- [ ] **AGENT-03** Implement only after the remaining sandbox and M15 real
  evaluation gates: effect-fenced cancellation, scoped context/tool surface,
  crash/restart reconciliation, parent root transfer, and strict aggregate
  token/process/time limits.
- [ ] **EVAL-AGENT-01** Compare one actor versus one actor + bounded children
  under the same total token/time/tool budget. Require better task success or
  latency, complete handoff provenance, zero authority escalation/stale
  effects, and bounded coordinator overhead before enabling by default.

### Gate 4: extension packaging

- [x] **ECO-03** Define a versioned plugin package manifest with explicit contributed
  tools, skills, hooks, adapters, dependencies, permissions, schemas, tests,
  and compatibility range.
  **Done 2026-08-12.** `PluginPackageManifest` (agent-contracts `plugin`)
  is the versioned installation unit: identity + version + name + summary,
  an `api` compatibility range (a lexically validated `VersionRange`;
  resolution is the installer's job, ECO-04), contributed `tools` (the
  capability plane's `ToolSpec` — same schema shape and caps), declared
  `skills` / `hooks` / `adapters` (metadata only per ECO-01: versioned,
  validated, never executed, never permission-bearing), `dependencies`
  (package id + version range), `permissions` (the known permission-word
  table), and `tests` (bounded argv self-checks run in a sandbox at
  install/test time, never during a turn). Admission is a core decision:
  `PluginPackageAdmission::validate_static` (agent-core `plugin_admission`)
  is a pure function of the manifest — id/version/range/name/summary
  shape, per-component id and duplicate checks, skill-reference
  confinement (package-relative, no absolute/`..`/backslash/control
  chars), hook-event shape, adapter endpoint bounds, dependency id+range,
  permission-word whitelist, test-command argv bounds, and per-package
  component caps (`MAX_TOOLS_PER_PACKAGE` 32, skills 16, hooks 16,
  adapters 8, dependencies 16, tests 16). Tool schemas reuse the capability
  schema validator verbatim, so a package cannot smuggle in a schema the
  capability plane would refuse. Nine admission tests pin the contract,
  including serde round-trip.
- [x] **ECO-04** Add install/inspect/test/enable/disable/quarantine flows; installation
  never implies activation or permission.
  **Done 2026-08-12.** `PluginRegistry` (agent-runtime `plugin`) owns the
  installed-package catalog and the lifecycle flows; the decisions behind
  them stay in the core. Admission is `PluginPackageAdmission`
  (ECO-03); activation is `PluginStateAuthority` (agent-core
  `plugin_state`), a validated state machine —
  `Installed → Active → Disabled ⇄ Quarantined` — where `Installed` is a
  terminal install-time state, quarantine may only leave through
  `Disabled` (a human step, never straight back to `Active`), and every
  transition is refused with a reason. `install` runs admission first,
  then registers the package **inert** (`Installed`): installing never
  implies activation or permission; `inspect`/`list` are bounded views;
  `enable`/`disable`/`quarantine`/`unquarantine` drive the validated
  transitions; `test` runs the declared self-checks in a sandboxed shape —
  private temp cwd, scrubbed environment (only `PATH` + platform
  essentials; a planted secret test proves nothing leaks), bounded timeout
  (default 30 s) with whole-tree kill on timeout (Windows `taskkill /T /F`,
  Unix own process group + negative-pid kill), bounded output tail, and no
  shell parsing — the core never runs a package's test command during a
  turn. 8 registry tests + 5 authority tests pin the flows, including
  duplicate-install refusal, unknown-package refusal, ordered transitions,
  pass/fail/timeout self-checks and the env scrub.
- [x] **ECO-05** Add MCP-like adapter support behind the same capability/effect/output
  boundary; lazily expose schemas through the existing catalog.
  **Done 2026-08-12.** `McpCapabilityAdapter` (agent-capability-process
  `mcp`) turns an MCP server over stdio (JSON-RPC 2.0, one document per
  line) into a `Capability` behind the exact same boundary as any other
  capability. `McpClient` is a minimal generic JSON-RPC client (id-matched
  responses, server notifications skipped, per-request timeout, frame-size
  bound, clean errors for closed/oversized/malformed frames) unit-tested
  against in-memory duplex streams; `connect_stdio` spawns the server with
  a scrubbed environment (PATH + platform essentials only, no
  secrets/HOME), a private temp cwd and discarded stderr, then runs the
  `initialize` handshake with protocol-version check. `connect()` performs
  one discovery pass (`tools/list`) and builds a static manifest whose
  schemas enter the existing catalog — loaded on demand through
  `capability.manage`, never injected wholesale into a request; `risk` is
  the caller's classification (derived from declared permissions, never
  self-declared by the server). Every invocation is forwarded as
  `tools/call`; the concatenated text content is clipped to
  `MAX_MCP_TOOL_TEXT_CHARS` before it reaches the model, and a server
  `isError` surfaces as `ok:false`, not a transport error. The server
  child is restarted lazily if the connection dropped between calls. Nine
  tests pin the protocol and the boundary, including a real spawned mock
  MCP server (`mcp_mock_server` bin) exercising discovery + invocation +
  failure + bounded echo.
- [x] **ECO-06** Define Skill activation/deactivation and provenance without turning
  instructions into System-authority content.
  **Done 2026-08-12.** `SkillDeclaration` (agent-contracts `plugin`) gains
  `provenance: SkillSource` (Builtin/Package/Operator — attribution only,
  never a permission source) and `activation: SkillActivation`
  (Active/Inactive, default Inactive). `PluginRegistry` (agent-runtime
  `plugin`) exposes a bounded `skills(package) -> Vec<SkillView>` metadata
  view (id/version/summary/reference/provenance/activation) plus
  `activate_skill`/`deactivate_skill`, which only record the operator's
  intent — activation changes no tool surface, starts nothing and never
  turns the referenced instructions into System-authority content. An
  anchor test installs a package whose skill reference points at a real
  file containing a marker and asserts no registry view ever serializes
  the marker; a second test asserts activation is intent-only (surface and
  activation untouched, unknown skill/package refused).
- [x] **ECO-07** Define Hook ordering, time/resource bounds, failure policy, and the rule
  that hooks cannot widen permissions or mutate protected state silently.
  **Done 2026-08-12.** Hooks stay declared metadata (ECO-01) but the
  firing contract is now pinned in the declaration and validated at
  admission. `HookDeclaration` (agent-contracts `plugin`) gains `order`
  (explicit priority), `timeout_ms` / `output_budget_chars` (bounded
  budgets, hard-capped at admission by `MAX_HOOK_TIMEOUT_MS` /
  `MAX_HOOK_OUTPUT_CHARS`), `failure: HookFailurePolicy`
  (RecordAndContinue / DenyOnFailure) and a per-hook `permissions` set.
  Admission (`PluginPackageAdmission`) refuses any event outside the
  `KNOWN_HOOK_EVENTS` vocabulary, any budget above the cap or zero, a
  failure policy inconsistent with the mode (a gate that records-and-
  continues is a silent fail-open and is refused outright; observers
  record-and-continue, gates fail closed), and any hook permission that is
  unknown or not a subset of the package's own permissions — a hook can
  never widen the package's set. `PluginRegistry` exposes bounded
  `hooks(package)` views and `hook_order(event)`, the deterministic firing
  order across every *active* package (ascending `order`, then package
  install order, then declaration order), so a future first-class hook
  runtime has the shape validated at install and needs no new authority.
  Seven tests pin the contract: unknown events, zero/over-cap budgets,
  mode/policy mismatch, out-of-set permissions, a valid gate hook, and the
  ordering rules including disabled/quarantined exclusion.

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

# ACI Contract v2 — Draft (TOOLS-02)

Status: **specification draft, no runtime behavior change**. 2026-08-11.

This document drafts the Tool contract v2 shapes promised by Gate 1 item
**TOOLS-02** of `docs/TOOL_ECOSYSTEM_TODO.md`:

1. the `ModelToolSpec` / `HostToolPolicy` split;
2. concrete `EffectIntent` / `EffectReceipt`;
3. `PermissionSet` and the standing-grant contract.

It deliberately changes no runtime behavior. Every type below is proposed;
the code (`agent-contracts`, `agent-kernel`, `tool-runtime`,
`agent-workspace`) remains authoritative until a later migration item
(`TOOLS-03`, `MOD-04`, `MOD-05`) makes a code change under the
compatibility order in `docs/TOOL_ECOSYSTEM_TODO.md` ("Compatibility order",
"Tool contract v2").

Inputs: `docs/TOOL_ECOSYSTEM_TODO.md` (contract v2 section, autonomy section,
incremental Core migration), `docs/TOOL_INVENTORY.json` (TOOLS-01, the
machine-readable inventory of the ten current tools), and the current
contract types in `agent-contracts/src/{tool,approval,capability}.rs`.

## 1. Vocabulary

The minimum v2 vocabulary stays intentionally small (unchanged from the
TODO document):

```text
ToolRegistration = ModelToolSpec + HostToolPolicy
EffectIntent     = normalized operation/target before execution
AuthorityLease   = Core-issued authority for one operation/generation
EffectRequest    = staged workspace/process/adapter action
EffectReceipt    = NotApplied | Applied | Unknown + durability/evidence
OutputBroker     = the only path from producer output to ToolOutput
PermissionSet    = typed, capability-level authority request (replaces free strings)
```

Rules that constrain every shape below:

- a tool/capability declaration is a **request and scheduling hint**, never
  the final security fact;
- Core computes the effective permission/risk from transport, granted
  handles, and the actual `EffectIntent`;
- all model-facing strings keep a global enforced cap even when a tool
  omits its local limit;
- large bodies are stored once and referenced, not cloned through events,
  TurnFrame, context, and artifacts;
- cancellation and timeout semantics are part of compatibility, not
  optional documentation;
- every mutation returns durable evidence sufficient to review and recover.

## 0. What has landed since this draft

**2026-08-11 (M14 slice):** the `EffectIntent` type is implemented in
`agent-contracts` (`ReadOnly | WorkspaceWrite { path, content_bytes } |
ProcessRun { command }`, with `risk()` bridging to the legacy `ToolRisk`),
and `TaskApprovalGate` now derives the concrete intent from the validated
arguments (`derive_effect_intent`) and matches standing grants against that
intent — approval is effect-derived, never tool-name-derived. The
`AuthorityGate` shadow-mode migration (`MOD-04`) can now reuse the derived
intent directly. Still open from this draft: commit-time resource
enforcement from the intent (the `AuthorityLease` step), the
`ModelToolSpec`/`HostToolPolicy` split, and typed `PermissionSet`/
`GrantSpec` — all remain specification until a later migration item.

## 2. ModelToolSpec / HostToolPolicy split

### 2.1 Why split

The current `ToolSpec { name, description, input_schema, risk,
output_budget }` mixes model-visible schema with host-only enforcement
metadata. The v2 split keeps security detail out of the prompt and out of
model-editable state, and gives `risk` a home where Core can compute a
derived lower bound instead of trusting a self-declared label.

### 2.2 ModelToolSpec (what the model sees)

```rust
/// Stable name, concise description, bounded input schema. This is the
/// only projection a provider/prompt may see.
pub struct ModelToolSpec {
    /// Stable tool id; exact up to MAX_TOOL_REQUIREMENT_NAME_CHARS.
    pub name: String,
    /// Concise description, capped (same ceiling the broker applies today).
    pub description: String,
    /// Bounded JSON schema (registration-time size caps, see TOOLS-03).
    pub input_schema: serde_json::Value,
}
```

Projection rules:

- `ModelToolSpec` derives from `ToolRegistration` at snapshot time; a
  round uses one immutable `ToolSurfaceSnapshot` of registrations and
  projects providers from the exact same snapshot (already the pattern for
  `ToolSurfaceSnapshot` today).
- A host-only policy change (budget, permission) must **not** bump the
  model-visible schema; only `name`/`description`/`input_schema` changes
  count as schema churn for token accounting.
- No `risk`, no `output_budget`, no permission fields appear here.

### 2.3 HostToolPolicy (host-only enforcement)

```rust
pub struct HostToolPolicy {
    // ---- identity ----
    pub version: String,
    pub owner: ToolOwner,        // Builtin | Capability { capability_id }
    pub source: ToolSource,      // Builtin | Manifest | Adapter
    pub maturity: ToolMaturity,  // Experimental | Preview | Stable

    // ---- model/output contract ----
    /// Declared model-content cap (chars); the broker clamps to the global
    /// cap (carries over today's ToolSpec.output_budget semantics).
    pub output_budget: Option<usize>,
    /// Declared summary cap; default = global MAX_TOOL_SUMMARY_CHARS.
    pub summary_budget: Option<usize>,
    /// When oversized content spills to an artifact.
    pub spill_policy: SpillPolicy, // Threshold | Always | Never

    // ---- authority contract ----
    /// The permission envelope this tool requests (see section 4).
    pub required_permissions: PermissionSet,
    /// Declared effect classes the tool may stage (see section 3.2).
    pub effect_kinds: Vec<EffectKind>,
    /// Core-derived lower bound on the effect class. A declaration may
    /// request *less* authority than its effect implies; Core raises the
    /// bound. It can never be lowered by the declaration.
    pub risk_lower_bound: ToolRisk,
    /// Workspace scope for writes: None | Prefix { path } | WholeWorkspace.
    pub workspace_scope: WorkspaceScope,
    /// Network scope: None | Allowlist { hosts } | Unrestricted.
    pub network_scope: NetworkScope,
    /// Credential/secret access: None | Scoped { keys } | Refused.
    pub credential_policy: CredentialPolicy,

    // ---- execution contract ----
    pub timeout_ms: Option<u64>,
    pub cancelable: bool,
    pub concurrency_limit: Option<u32>,
    pub idempotency: Idempotency,   // None | Key { key_arg } | Natural
    pub retry: RetryPolicy,         // Never | Limited { max, backoff_ms }

    // ---- context contract ----
    /// Existing ToolResultDisposition: PersistObservation |
    /// TransientNoPersist | AccessEventOnly.
    pub disposition: ToolResultDisposition,
    /// What evidence a result may carry: None | ArtifactRef | Structured.
    pub evidence_kind: EvidenceKind,

    // ---- audit contract ----
    /// Bounded start/finish/effect events, resource accounting, generation.
    pub audit: AuditPolicy,
}
```

Defaults are the conservative ones: `None`/`Never`/`Refused` unless the
tool declares otherwise, and Core still verifies the declaration against
the actual `EffectIntent` at invocation time.

### 2.4 ToolRegistration

```rust
pub struct ToolRegistration {
    pub model: ModelToolSpec,
    pub host: HostToolPolicy,
    /// Registration-time identity/revision assigned by the registry.
    pub registration_id: String,
    /// Monotonic registry revision that created this registration.
    pub catalog_generation: u64,
}
```

Current-code mapping (proposed conversion, conservative):

| v2 field | today | conversion |
| --- | --- | --- |
| `ModelToolSpec.name` | `ToolSpec.name` | direct |
| `ModelToolSpec.description` | `ToolSpec.description` | direct |
| `ModelToolSpec.input_schema` | `ToolSpec.input_schema` | direct |
| `HostToolPolicy.output_budget` | `ToolSpec.output_budget` | direct |
| `HostToolPolicy.disposition` | actor-side disposition map (`actor.rs`) | direct (stays runtime-owned) |
| `HostToolPolicy.risk_lower_bound` | `ToolSpec.risk` | direct; Core may raise |
| `HostToolPolicy.effect_kinds` | `ToolSpec.risk` | 1:1 seed: ReadOnly→reads, WorkspaceWrite→workspace writes, ProcessExecution→process runs |
| `HostToolPolicy.required_permissions` | `CapabilityManifest.permissions: Vec<String>` | parse known prefixes (`workspace:read` etc.); unknown strings → registration rejected or permission dropped with an explicit reason |
| everything else | absent | conservative default |

## 3. EffectIntent / EffectReceipt

### 3.1 EffectIntent — the normalized operation before execution

The Core derives a concrete `EffectIntent` from the host policy plus
validated arguments. Approval/policy matches this concrete intent, never
merely the tool name.

```rust
pub struct EffectIntent {
    /// What kind of side effect: workspace write, patch apply, process run,
    /// network request, outbox send, artifact write, ...
    pub kind: EffectKind,
    /// Typed target: a workspace-relative path, an argv prefix, a URL.
    pub target: EffectTarget,
    /// How reversible the effect is after commit.
    pub reversibility: Reversibility, // Reversible { rollback_ref } | Journaled | Irreversible
    /// Idempotency key when the producer can provide one (or `Natural`
    /// when the operation is naturally idempotent).
    pub idempotency: Idempotency,
    /// Retry semantics derived from policy + operation.
    pub retry: RetryPolicy,
    /// Conservative resource estimate: content bytes, runs, wall-clock ms.
    pub resource_estimate: ResourceEstimate,
    /// Who staged this intent: tool/capability id + invocation id.
    pub origin: EffectOrigin,
}
```

Derivation rules:

- `EffectIntent` is produced **before** execution from validated arguments;
  the executor's actual target is canonicalized after preparation (step 4
  of the six-step flow, section 3.3).
- The intent is an **upper bound**: if preparation discovers a wider target,
  the operation rolls back and needs a new authorization; it never silently
  widens the lease.
- `EffectKind` and `EffectTarget` are closed enums at first (the wire
  vocabulary stays small); an adapter maps its opaque behavior onto these,
  or refuses to run.

### 3.2 EffectRequest — the staged action

`EffectRequest` already exists as the capability staging variant
(`CapabilityOutcome::EffectRequest { output, effect: Box<dyn Effect> }`,
`agent-contracts/src/capability.rs`) and as the builtin
`ToolOutcome::PreparedEffect { output, effect }`. v2 keeps it as the only
mutation path:

- a capability/builtin **computes**, Core **executes** behind the
  generation fence;
- `WireEffect` (today: `WorkspaceWrite { path, content_b64 }`) is the
  process-boundary serialization of an `EffectRequest`; v2 may add
  `WorkspacePatch`, `ProcessRun`, `NetworkRequest` variants, each still
  staged through confined handles and committed by Core only.

### 3.3 Six-step authorization flow (unchanged, now concrete)

```text
1. validate arguments + derive a conservative EffectIntent upper bound
2. standing policy/AuthorityGate -> short-lived AuthorityLease
3. prepare using only lease-scoped handles and Core staging; target world unchanged
4. canonicalize actual targets and prove actual intent is within the lease
5. actor checks generation/cancellation; Core commits or rolls back
6. emit EffectReceipt; OutputBroker bounds/spills the final result
```

The post-prepare check (step 4) is **not** a second confirmation prompt; it
is a proof that the actual effect fits the issued lease. Today's
`TaskApprovalGate::grant_matches` already does argument-level matching
(path/command/content against the grant target) — that logic becomes the
`AuthorityGate` intent matcher, generalized from `ToolCall` arguments to a
typed `EffectIntent`.

### 3.4 EffectReceipt

```rust
pub enum EffectOutcome {
    /// Nothing landed; world unchanged.
    NotApplied,
    /// The effect landed; the journal recorded it.
    Applied,
    /// The effect may have landed but the outcome is unknowable (remote
    /// call failed after send, no idempotency key). Never blindly retried.
    Unknown,
}

pub struct EffectReceipt {
    pub outcome: EffectOutcome,
    /// Stable external/change id for review and recovery.
    pub effect_ref: String,
    pub idempotency_key: Option<String>,
    pub reversibility: Reversibility,
    /// Artifact/journal references proving what happened.
    pub evidence_refs: Vec<String>,
    /// Journal durability: Journaled | JournalFailed. JournalFailed means
    /// the effect landed but its record did not — degraded/recovery state,
    /// never "nothing happened" (carries over EffectCommitError).
    pub durability: Durability,
    /// Why: "committed because generation N current and lease L valid",
    /// "rolled back because operation cancelled" — keeps every effect
    /// explainable.
    pub reason: String,
}
```

Mapping to today's `EffectCommitError`:

| `EffectCommitError` (today) | `EffectReceipt` (v2) |
| --- | --- |
| `Ok(())` | `Applied` + `Journaled` |
| `NotApplied(err)` | `NotApplied` + reason from `err` |
| `AppliedButDurabilityFailed(err)` | `Applied` + `JournalFailed` + recovery flag |

## 4. PermissionSet

`CapabilityManifest.permissions: Vec<String>` is too coarse: a capability
can understate real behavior with free strings. v2 replaces it with a
typed envelope that Core computes the effective form of.

```rust
pub struct PermissionSet {
    /// None | Read | Write { confined } | WriteAnywhere
    pub workspace: WorkspacePermission,
    /// None | Run { argv_prefix, sandbox_class } | Unrestricted
    pub process: ProcessPermission,
    /// None | Connect { host_allowlist } | Unrestricted
    pub network: NetworkPermission,
    /// None | Write { run_artifact_dir }
    pub artifact: ArtifactPermission,
    /// Context-control authority (today's RUNTIME_CONTEXT_CONTROL gate).
    pub context_control: bool,
    /// None | Scoped { key_ids }
    pub credentials: CredentialPermission,
    /// Task authority (task.manage / task.complete style directives).
    /// Never granted to an ordinary plugin by declaration alone.
    pub task_authority: bool,
}
```

Rules:

- a declaration is a **request**: `PermissionSet` requests the ceiling, and
  Core derives the effective permission from the transport (in-process vs
  process vs MCP adapter), the granted handles, and the actual
  `EffectIntent`;
- extensions may request less authority; they can never grant themselves
  more — a `deny`/`ask` constraint contributed by an extension never adds
  an `allow` above administrator/user/Core policy;
- unknown permission strings at registration are rejected with an explicit
  reason (no silent wildcard), mirroring today's manifest validation;
- the mapping from current strings is deterministic:
  `workspace:read` → `Workspace::Read`; `workspace:write` → `Workspace::Write{confined}`;
  `process:run` → `Process::Run{sandbox}`; `artifact:write` → `Artifact::Write`;
  `runtime:context-control` → `context_control = true`.

## 5. Standing grant v2 (GrantSpec)

Today's `StandingGrant { id, risk, target: GrantTarget, constraint:
GrantConstraint, expires_at_ms }` matches at the `ToolRisk` level. v2
matches at the `PermissionSet`/`EffectIntent` level so a grant can be
narrower than a tool's declared class without inventing new tool names.

```rust
pub struct GrantSpec {
    pub id: String,
    /// What authority this grant confers (typed, not ToolRisk).
    pub grants: PermissionSet,
    /// Target scope: workspace prefix, command prefix, network allowlist.
    /// At least one scope must be set (today's GrantTarget, extended).
    pub target: GrantTargetV2,
    /// Resource envelope (today's GrantConstraint, extended).
    pub constraint: GrantConstraintV2,
    /// Task | Capability | Global scope of the grant.
    pub scope: GrantScope,
    /// Expiry epoch ms; inert at/after expiry.
    pub expires_at_ms: u64,
}

pub struct GrantTargetV2 {
    pub workspace_path_prefix: Option<String>,
    pub process_command_prefix: Option<String>,
    pub network_host_allowlist: Option<Vec<String>>,
}

pub struct GrantConstraintV2 {
    pub max_content_bytes: Option<u64>,
    pub max_runs: Option<u32>,
    pub max_total_bytes: Option<u64>,   // cumulative across covered calls
    pub max_wall_ms: Option<u64>,       // cumulative wall time across covered calls
}
```

Invariants (unchanged from today's `TaskApprovalGate`, generalized):

- grants are established by the composition root / UI only; the model can
  **use** a matching grant but can never create, widen, or extend one;
- grants only shrink: revocation, run consumption, expiry; an expired /
  revoked / exceeded grant silently stops matching and the call falls
  through to the underlying gate;
- a grant is an approval decision, **not** a sandbox bypass — the confined
  workspace (CORE-07), the generation fence, and `OutputBroker` still apply;
- legacy wildcard/unknown shell or process intents never qualify for
  automatic standing-grant execution;
- an extension may contribute `deny`/`ask` constraints but never an `allow`
  above policy.

Mapping from today's `StandingGrant`: `risk` maps to the coarsest
`PermissionSet` whose only permission is that class (WorkspaceWrite →
`{ workspace: Write{confined} }`; ProcessExecution →
`{ process: Run{ argv_prefix } }`), `target`/`constraint` map field-for-field,
`expires_at_ms`/`id` direct. A `GrantSpec` with the old shape is exactly
what today's `TaskApprovalGate` accepts; nothing about the runtime behavior
changes until `MOD-04`/`MOD-05` wire the new matcher in shadow mode.

## 6. AuthorityLease

One operation/generation, short-lived, Core-issued:

```rust
pub struct AuthorityLease {
    pub lease_id: String,
    /// Actor-owned operation generation the lease is valid for. Core
    /// validates the supplied lease is current before commit (stale
    /// generation => rollback, never commit).
    pub operation_generation: u64,
    /// The approved concrete intent (upper bound).
    pub intent: EffectIntent,
    /// Which standing grant covered the decision, if any.
    pub grant_id: Option<String>,
    pub decision: ApprovalDecision,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
```

Lifecycle: issued at step 2 of the six-step flow; consumed (or rolled back)
at step 5; invalid after expiry or after the operation generation advances.
A `RuntimeCheckpoint` may request a previous tool surface but can never
restore a stale lease, raise maturity, enable a disabled capability, or undo
a later quarantine (existing checkpoint invariant).

## 7. Compatibility order (field-level notes)

The TODO document's compatibility order is the migration plan; this section
only pins the field conversions each step must preserve:

1. **DTOs + conservative conversion** — `ToolSpec -> ToolRegistration`
   per section 2.4; all new types serde-versioned; no model-facing change.
2. **Snapshots to registrations** — `ToolSurfaceSnapshot` carries
   `ToolRegistration`; providers/prompt project only `ModelToolSpec`.
3. **Broker existing outputs** — unchanged `OutputBroker::bound`, now fed
   `HostToolPolicy.output_budget`/`summary_budget` instead of
   `ToolSpec.output_budget` (identical clamp semantics).
4. **AuthorityGate in shadow mode** — new intent matcher runs beside the
   legacy `ApprovalGate`; decisions logged, not enforced, until the
   invariant trace (granted/denied/reason) matches the legacy path.
   **Landed 2026-08-11 (MOD-04 slice).** `IntentShadowGate` + `ShadowVerdict`
   (agent-contracts), the `AgentKernelConfig.shadow_gate` injection, and a
   `RuntimeEvent::ShadowDecision { call_name, legacy_allowed, shadow }`
   published by `execute_tool` for allowed and denied calls alike. The
   standing-grant gate implements the shadow verdict with the *same*
   matching logic as its legacy `authorize` (derived `EffectIntent` against
   live grants, including the run cap) without consuming any state, so the
   hard invariant — shadow `Granted` implies legacy `Allow` — holds by
   construction and is pinned by tests
   (`shadow_verdict_never_grants_beyond_the_legacy_path`,
   `execute_tool_publishes_the_shadow_decision_event`). An ungranted
   write/process call is shadow-`Denied` even when the legacy inner gate
   would allow it — shadow being stricter than legacy is the point; the
   reverse would be a privilege-expansion bug. The TUI renders the bounded
   verdict row.
5. **Migrate builtin effects + receipts** — builtin workspace effects emit
   `EffectReceipt`; journal format keeps `EffectCommitError` semantics.
   **Landed 2026-08-11 (MOD-04 slice).** `Effect::commit` now returns the
   serializable `EffectReceipt` (`NotApplied{error}` / `Applied{
   durability: Durable | DurabilityFailed, evidence}` / `Unknown{error}`)
   instead of `Result<(), EffectCommitError>`. The workspace
   `PreparedMutation` emits `Applied` with its transaction id as evidence;
   the composite `Vec<Box<dyn Effect>>` aggregates sub-effect evidence and
   stops at the first non-durable receipt; the actor matches the receipt to
   build the same model-facing messages as before (`DurabilityFailed` keeps
   the "WAS applied but the journal failed — recovery required" path, and
   `Unknown` is the new never-blindly-retry branch for remote effects).
   `EffectCommitError` remains as the internal error space and converts
   one-to-one into receipts; the journal format is unchanged.
6. **Disable direct capability mutation, add IPC EffectRequest** — the
   `WorkspaceHandle::write` direct path is removed; process children submit
   `WireEffect` (already the shape) and receive `EffectReceipt` over IPC.
7. **Sandboxed shell/process, read-only then mutating MCP adapters** — last.

Explicitly out of scope for this migration (unchanged from the TODO): no
policy DSL, no LLM risk classifier, no generic distributed transaction, no
automatic arbitrary-MCP writes, no WASM ABI, no marketplace, no output
schema, no learned tool selection.

## 8. Open questions (draft honesty)

- **PermissionSet granularity**: closed enums vs open allowlists for hosts
  and credential keys; the draft assumes closed enums plus allowlists for
  network/credentials only.
- **Idempotency on non-idempotent shells**: `shell.exec` has no natural
  idempotency key; v2 requires `Idempotency::None` + `RetryPolicy::Never`
  unless the tool declares a key argument. Whether a later structured
  process protocol (`TOOLS-06`) adds a per-run key is deferred.
- **Upper-bound derivation precision**: `EffectIntent` is derived from
  validated JSON arguments; for free-form shell commands the upper bound
  must stay the command prefix (today's `grant_matches` logic). A finer
  bound needs the structured process protocol, not a smarter parser.
- **Where `PermissionSet` lives**: `agent-contracts` (shared) vs
  `agent-kernel` authority module; draft assumes contracts so both the
  manifest and the Core matcher share one shape.

## 9. Definition of done for this draft

TOOLS-02 is closed when all of the following hold (this commit):

- [x] split `ModelToolSpec` / `HostToolPolicy` specified field-by-field;
- [x] `EffectIntent` / `EffectReceipt` specified with the `Unknown` outcome
      and durability semantics;
- [x] `PermissionSet` specified with deterministic mapping from today's
      manifest permission strings;
- [x] standing-grant contract v2 (`GrantSpec`) specified with a mapping
      from today's `StandingGrant`;
- [x] compatibility order pinned to field conversions;
- [x] no runtime behavior changed (documentation-only commit);
- [ ] open questions resolved in a later item (`TOOLS-03` error/result
      envelopes, `MOD-04`/`MOD-05` authority slices) — tracked here so the
      draft does not pretend to be final.

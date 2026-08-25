# Platform security (M12 / M13 first cuts)

Neither M12 nor M13 is closed. This file is the authority/sandbox contract.
Layering and crate graph live in [`ARCHITECTURE.md`](ARCHITECTURE.md).
Current freeze / P0 live in [`STATUS.md`](STATUS.md).

Generic `shell.exec` / `process.run` / `process.session`, plus the bounded
host-recipe `verify.run`, stay a non-transactional exception: Core identity before spawn, kill-then-reap
on cancel, no rollback of child mutations. Do not invent `MOD-18` from
residual syscalls, multiplexing, or Named Pipe/UDS. WASI is a V2
untrusted-plane candidate, not a v0 slice.

## ToolSpec is not authority

`ToolSpec` is the model-visible schema. Trusted `HostToolPolicy` binds
builtin arguments to a real [`EffectIntent`]. A plugin cannot
self-authorize via `ToolRisk` plus parameter names (`command`, `argv`,
`destination`, `payload`). Unknown names fail closed: ProcessExecution
becomes an empty `ExecArgv`; WorkspaceWrite becomes an empty path / 0
bytes.

Layering (CORE-11, landed 2026-08-23): `agent-contracts/src/host_policy.rs`
defines the vocabulary and the `HostToolPolicies` lookup trait whose
`effect_intent` derivation every consumer shares; the builtin table lives
in `tool-runtime` (`BuiltinToolPolicies`); trusted composition owns
`agent-compose::HostToolPolicyRegistry`, which admits operator-reviewed
plugin bindings and never lets them shadow a builtin. The same registry
reaches the kernel lease path, the approval gate, and the capability
dispatcher — approval and lease minting cannot drift. The admission flow is
explicit: an installed package manifest contributes only candidate tool
names, while the operator review artifact supplies the actual
`HostToolPolicy` bindings (including argument-name binding);
`admit_reviewed` installs them atomically — any builtin shadow, duplicate
admission, or tool outside the reviewed manifest refuses the whole batch.
`revoke_admitted` withdraws a previously admitted binding; builtins are
irrevocable. Both moves advance the snapshot revision and digest, and
consumers holding the old snapshot keep its exact authority, so an operator
update never re-interprets an in-flight operation. Still open (M12):
fencing already-minted leases when their tool's binding is revoked
mid-flight via an explicit per-binding revocation epoch.

`HostEffectBinding::ExecRecipe` is the trusted indirection for
`verify.run { recipe_id }`: the model-visible call contains no argv, and the
composition root installs the exact bounded `id -> argv` table as an immutable
builtin extension. The dispatcher executes from that same recipe set. Unknown
ids resolve to empty `ExecArgv`; a plugin cannot shadow `verify.run` or add an
argv through `ToolSpec`/call metadata. The resolved spawn is still checked as
Actual ExecArgv ⊆ Approved ExecArgv before the child starts.

Exact PASS reuse is narrower than spawn authority. A source-read-only recipe
must supply a complete bounded host identity; Runtime recomputes it after the
process and records exact provenance only when pre/post identities match.
Incomplete workspace/environment capture, links or external-input directives
retain typed verification but cannot authorize a later no-dispatch PASS.

## EffectIntent (M12 first cut)

```text
ReadOnly
WorkspaceWrite { path, content_bytes }          // one exact target
WorkspaceWriteSet { writes }                    // MOD-AUTH-01: every target of one multi-file call
ExecArgv { program, argv }     // argv prefix; intact argument boundaries
ShellExec { dialect, command_digest }  // exact JCS+sha256 of {"command"}
```

`WorkspaceWriteSet` carries one `WorkspaceWriteBound { path, max_bytes }`
per target — each file gets its own approval-time byte estimate — and is
hard-capped at `MAX_WORKSPACE_WRITE_SET` (16 entries, matching
`edit.patch`'s per-call file cap). For `edit.patch` the estimate is the
per-file hunk delta: a *lower bound* on the final body, never the real
write size; real-byte resource caps are enforced by the workspace
mutation itself, and commit-time path containment by MOD-AUTH-02 below.

Whitespace token prefixes are not a grant. A standing `git status` grant
must not cover `git status && …`. `process.run` must not join argv and
re-split: `["git", "status && evil"]` is one argument, not three tokens.

**MOD-AUTH-01 (multi-resource authority widening, fixed 2026-08-21).**
`edit.patch files[]` used to derive a single-path intent from the first
entry, so a `src/` standing grant authorized writes to every other file
in the set while the executor staged up to 16 real write targets. The
host policy now derives `WorkspaceWriteSet` (every distinct
`files[].path`, each with its own byte estimate) whenever a call names
more than one target; a single-file call keeps the single-resource
shape so existing grants still match exactly. `grant_matches` requires
**every** path inside the grant prefix; a content cap must cover both
the summed per-entry estimates and each entry individually. `covers`
accepts set equality (or one member for a single write) — one approved
path never widens into a set, and `covers` is fail-closed on any
uncanonicalizable member. Knowledge-plane touches
(`metadata.files[].path`) already carried every target; the authority
intent now matches that shape.

**MOD-AUTH-02 (commit-time Actual ⊆ Approved, fixed 2026-08-21).**
Approval bounds are estimates; commit is checked against what was
actually staged. Trusted builtin write effects implement
`Effect::actual_workspace_writes()`, returning canonical
`ActualWorkspaceWrite { path, bytes }` — the real relative path and the
real staged byte count (for `edit.patch`, the final body size, not the
hunk delta). Composite `Vec<Box<dyn Effect>>` aggregates children and
reports `None` if any child cannot describe its targets (never a
guessed partial set). Core `commit_effect` compares the actual set
against the lease's approved paths (slash/backslash canonicalized) and
rejects with `ActualExceedsApproved` + rollback when an actual path
falls outside the approved intent. Effects that cannot conservatively
describe their targets (default `None`) skip the check — only trusted
builtins report `Some`. The flow is prompt-soft / runtime-hard:

```text
tool arguments → HostToolPolicy → approved intent
  → tool computes → actual effect set
  → Core checks approved ⊇ actual → commit
```

The same commit boundary carries a content TOCTOU guard. For existing edit targets,
`Workspace::begin_existing_mutations` acquires canonical path-keyed leases
in sorted batch order before taking one exact bounded snapshot; that one scan
feeds transformation, SHA-256, recovery hash and bounded old-content capture.
Every child retains the shared lease group through composite settlement, so
same-`Workspace` writers cannot enter between snapshot, check and replace.
For Core-managed effects, the synced authority-journal v2 intent precedes temp
creation and carries the deterministic staged name, bounded byte lengths and
SHA-256 before/after revisions. Reconciliation uses confined handles, bounded
reads and content/name revalidation; it deletes only a stage proven to be the
owned complete after-state. File effects require an already-existing parent;
they cannot create unjournaled directory topology before this intent.
Unprovable stage state is `Ambiguous`, not an unsafe cleanup. Typed rollback
propagates cleanup/terminal uncertainty to the Core recovery fence.

The prepared effect still re-hashes immediately before atomic rename: drift
from a writer outside that lease already visible when the check begins cleans
the staged file and settles as `NotApplied` (`stale_revision`). This is not an
atomic filesystem CAS against direct or authority-bypassing filesystem
writers; they can still race between hash and rename. A second official
`Workspace::open` on the same root is instead refused by the exclusive
authority-journal lock. The content precondition and in-process lease
complement, but do not replace, the generation fence or Actual ⊆ Approved
authority check.

`process.session` start is `ExecArgv`. Poll/stop do not spawn and cannot
spend an argv-prefix grant. Session recovery is keyed by the **start**
identity.

Standing grants: a process grant is exactly one of `exec_argv_prefix` or
`shell_command_digest`. Spawn fails closed unless the approved bound
covers the actual spawn.

M12 remaining: one `EffectRequest`/commit path for brokerable side
effects. The reserved/dispatch/ack barrier is now structured in Core:
every approved effect crosses an `EffectBroker` seam — `reserve` before
anything applies (failure fences dispatch and settles the prepared effect
NotApplied as a `BrokerUnavailable` rejection), `dispatch` applies the
prepared effect exactly once under its reservation, and `ack` reports the
outcome without ever rolling an applied effect back. The default local
broker preserves inline behavior byte-for-byte; the operation terminal
record remains the durability barrier. A future HTTP/gRPC coordinator
implements the same three calls against this trait — that transport, plus
crash reconciliation of broker-owned reservations, is still M12 work.
Do not close M12 because structured intents or the local barrier landed.

## HostLifecycle (restart)

Process-connection state is not `Option<Host>`:

```text
NeverStarted | Serving(T) | Quarantined { reason } | Stopped
```

First connect after NeverStarted/Stopped is not a restart and must not
consume `RestartCircuit`. A failed replacement stays Quarantined and
must consume the circuit. Code: `crates/agent-process/src/health.rs`.

PLAT-06 slice 1 (health / epoch / circuit) and slice 2 (peer cancel-ACK
+ coalescible progress) are landed. Remaining PLAT-06 is multiplexing
(stay single-inflight). Named Pipe/UDS remain later (`PLAT-08`).

## SandboxProfile vs attestation (M13 first cut)

Manifests declare `SandboxProfile`. The host compares it to **actually
enforced** `SandboxCapabilities` after spawn, not to configured policy.
Activation: `required ⊆ actual`.

| Profile | Start rule |
| --- | --- |
| `Trusted` | Empty required set. Operator-installed. Missing OS fences may degrade. |
| `Restricted` | Write + memory + process count quota must be attested. |
| `UntrustedGenerated` | Read, write, TCP, UDP, Unix, spawn, signal, CPU, memory, fd — all ten flags. Native process cannot prove fs-read confinement / UDP / pathname-Unix today → **fail-closed**. WASI is the V2 candidate for this profile. |

`fs_read_confined` and `cpu_quota` are part of the untrusted floor
*now* (2026-08-21), before the OS planes can prove them: once UDP /
pathname-Unix denials land, a profile that forgot fs-read/CPU would
pass activation with absolute host reads and unlimited CPU still open —
a containment hole rented from the future. Requiring them today keeps
the fail-closed posture honest.

Attestation is computed from what was applied (landlock actually applied,
Windows job assigned, rlimits > 0). `false` means "not proven".

Every enforced flag now carries its mechanism:
`ProcessHost::sandbox_attestation` returns
`SandboxAttestation { capabilities, backend, backend_version, evidence }`.
`capabilities` stays the wire-compatible boolean floor that activation
consumes; `backend` names the OS family (`landlock+rlimits`,
`integrity+jobobject`, `rlimits`) with a probed landlock ABI level as the
Linux version; `evidence` explains each true flag from real enforcement
inputs (write-root counts, rlimit and job-object values) and refuses proof
for flags that are not enforced (`SandboxEvidence::consistent_with`,
checked by `validate()`). A boolean must still never claim more than it
delivers — `process_count_quota` remains a count quota, and
UntrustedGenerated stays fail-closed on native until the residual planes
(UDP / raw / pathname-Unix, absolute OS reads) can attest.

## Per-OS enforcement matrix

What v0 can currently *attest* as true. Blank / false = not proven.

| Capability | Linux | Windows | Unix non-Linux |
| --- | --- | --- | --- |
| `fs_write_confined` | Landlock write roots (`MOD-06`) | Low-IL labeled roots (`MOD-08`) | — |
| `fs_read_confined` | App-level broker only; no landlock read fence | App-level broker only | App-level broker only |
| `tcp_connect_denied` | Landlock ABI v4+ when write roots set (`MOD-07`) | — | — |
| `udp_denied` | — | — | — |
| `unix_socket_denied` | — | n/a | — |
| `process_count_quota` | `RLIMIT_NPROC` (user-level count quota) | Job Object assigned | `RLIMIT_NPROC` (user-level count quota) |
| `signal_scoped` | Landlock ABI v6 (`MOD-11`) | — | — |
| `cpu_quota` | `RLIMIT_CPU` | — | `RLIMIT_CPU` |
| `memory_quota` | `RLIMIT_AS` (`MOD-09`) | Job commit cap (`MOD-14`) | `RLIMIT_AS` |
| `fd_quota` | `RLIMIT_NOFILE` (`MOD-13`) | — | `RLIMIT_NOFILE` |

Also landed, not separate attestation flags: env scrub, private cwd,
bounded stderr, process-tree kill-then-reap, directory-handle-relative
workspace opens (`CORE-07`), mid-invoke `fs.read` brokered under
`workspace:read`, network deny-by-default at the broker, ABI v5 ioctl
deny (`MOD-12`), `RLIMIT_FSIZE` (`MOD-10`), `RLIMIT_CORE=0` (`MOD-15`),
Linux NICE/RTPRIO + `no_new_privs` (`MOD-16`), Windows Job
`PRIORITY_CLASS=NORMAL` (`MOD-17`).

`process_count_quota` was renamed from `process_spawn_controlled`
(2026-08-21; a serde alias keeps the wire compatible). `RLIMIT_NPROC`
is a user-level *count quota*, not proof that arbitrary spawning is
impossible and not a per-sandbox process namespace. A boolean must not
claim a stronger OS guarantee than it delivers; a true
spawn-denied/brokered floor is a separate future item.

Booleans are the v1 attestation floor. M13 acceptance should upgrade
them to `SandboxAttestation { capabilities, backend, backend_version,
evidence }` so each enforced capability is explainable
(`fs_write_confined` → landlock ABI, `memory_quota` → rlimit_as bytes,
`tcp_connect_denied` → landlock ABI v4+). Same principle as context
lifecycle: state why you believe the state is true.

## Residual (out of v0; not MOD-18)

- Linux UDP / raw / pathname-Unix
- Linux absolute OS-level reads
- Windows OS-level network
- I/O bandwidth quotas
- seccomp / AppContainer
- Multiplexing; Named Pipe/UDS

Untrusted generated code fails closed unless the host can attest the
required floor. Do not paper residual syscalls with a new MOD slice.

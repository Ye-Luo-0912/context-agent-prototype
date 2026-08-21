# Platform security (M12 / M13 first cuts)

Neither M12 nor M13 is closed. This file is the authority/sandbox contract.
Layering and crate graph live in [`ARCHITECTURE.md`](ARCHITECTURE.md).
Current freeze / P0 live in [`STATUS.md`](STATUS.md).

Generic `shell.exec` / `process.run` / `process.session` stay a
non-transactional exception: Core identity before spawn, kill-then-reap
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

Code: `crates/agent-contracts/src/host_policy.rs`.

## EffectIntent (M12 first cut)

```text
ReadOnly
WorkspaceWrite { path, content_bytes }
ExecArgv { program, argv }     // argv prefix; intact argument boundaries
ShellExec { dialect, command_digest }  // exact JCS+sha256 of {"command"}
```

Whitespace token prefixes are not a grant. A standing `git status` grant
must not cover `git status && …`. `process.run` must not join argv and
re-split: `["git", "status && evil"]` is one argument, not three tokens.

`process.session` start is `ExecArgv`. Poll/stop do not spawn and cannot
spend an argv-prefix grant. Session recovery is keyed by the **start**
identity.

Standing grants: a process grant is exactly one of `exec_argv_prefix` or
`shell_command_digest`. Spawn fails closed unless the approved bound
covers the actual spawn.

M12 remaining: one `EffectRequest`/commit path for brokerable side
effects. A future HTTP/gRPC broker still needs the reserved/dispatch/ack
barrier. Do not close M12 because structured intents landed.

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
| `Restricted` | Write + memory + spawn must be attested. |
| `UntrustedGenerated` | Write, TCP, UDP, Unix, spawn, signal, memory, fd. Native process cannot prove UDP / pathname-Unix / OS-read today → **fail-closed**. WASI is the V2 candidate for this profile. |

Attestation is computed from what was applied (landlock actually applied,
Windows job assigned, rlimits > 0). `false` means "not proven".

## Per-OS enforcement matrix

What v0 can currently *attest* as true. Blank / false = not proven.

| Capability | Linux | Windows | Unix non-Linux |
| --- | --- | --- | --- |
| `fs_write_confined` | Landlock write roots (`MOD-06`) | Low-IL labeled roots (`MOD-08`) | — |
| `fs_read_confined` | App-level broker only; no landlock read fence | App-level broker only | App-level broker only |
| `tcp_connect_denied` | Landlock ABI v4+ when write roots set (`MOD-07`) | — | — |
| `udp_denied` | — | — | — |
| `unix_socket_denied` | — | n/a | — |
| `process_spawn_controlled` | `RLIMIT_NPROC` | Job Object assigned | `RLIMIT_NPROC` |
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

## Residual (out of v0; not MOD-18)

- Linux UDP / raw / pathname-Unix
- Linux absolute OS-level reads
- Windows OS-level network
- I/O bandwidth quotas
- seccomp / AppContainer
- Multiplexing; Named Pipe/UDS

Untrusted generated code fails closed unless the host can attest the
required floor. Do not paper residual syscalls with a new MOD slice.

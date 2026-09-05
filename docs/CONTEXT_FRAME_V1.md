# Shadow Context Frame (V1 spec)

The structured Context Frame makes the model input *inspectable*: every
piece of state that reaches the model is a classified block with an
explicit zone, authority, freshness, requirement and representation —
and everything that did **not** reach the model is counted. V1 ships as
measurement only (Frame-0/1/2): the compiler never changes a byte of the
model input. This document is the spec those gates enforce.

## Pipeline

| Stage | Artifact | Where |
| --- | --- | --- |
| Frame-0 shadow compile | `FrameManifest` per model round, emitted as `RuntimeEvent::ContextFrameShadow` (bounded JSON, schema `context-frame-shadow/v1`) | `agent-runtime::frame`, gated by the `shadow_context_frame` compose flag |
| Frame-1 offline comparison | `agent-replay --frame-report <traces…>`: context-layer cost vs structured-frame cost per paired round | `agent-replay::frame_report` |
| Frame-2 scripted gate | `gate_shadow_frame` / `gate_manifest_invariants` + `agent-replay --frame-gate <traces…>` (non-zero exit on violation) | `agent-runtime::frame` |
| Frame-3 live gate | current vs structured frame on real serving, non-inferiority before any model-facing flip | future; waits for `LT-EVAL-06` cadence |

## Zones

| Zone | Authority | Content |
| --- | --- | --- |
| `task_contract` | operator boundary | goal, current interpretation, constraints, acceptance criteria (must-represent, bounded bodies) |
| `execution_state` | runtime trusted | advisory `next_action`, plan progress, open loops (must-represent), ack-debt descriptor |
| `current_evidence` | retrieved untrusted | foreground evidence pack bodies (prefer-body) |
| `working_memory` | retrieved untrusted | selected working set (prefer-body) |
| `external_directory` | retrieved untrusted | externalized refs — **descriptors only, never bodies** |

## Blocks and invariants

Every block carries: zone, authority, freshness, requirement,
representation, source locator, bounded content, token estimate, and the
SHA-256 of the *untruncated* content. Invariants (enforced by the
scripted gate, unit-tested as the review's Frame-2 matrix):

1. Mandatory coverage — every anchor goal/constraint/criterion appears
   as a TaskContract block unless the manifest cap omitted it (recorded
   in the zone stats, never silent).
2. Unresolved ack debts surface as a descriptor; required misses travel
   from the materialization unchanged.
3. No duplicate bodies: one full block per content digest; the rest are
   counted as `duplicates_removed`.
4. Bodies stay bounded (240 chars working-set / 600 chars contract).
5. External directory blocks are descriptors; external bodies are never
   auto-fetched into the frame.
6. Zone statistics always agree with the block list.
7. Determinism: identical inputs produce an identical `frame_digest`
   (SHA-256 over the canonical block set).
8. Engine-agnostic: the compiler consumes only the `MaterializedContext`
   contract, so baseline engines and the dynamic engine compile alike.

## The trap this avoids

"Complete" means **complete census, bounded bodies** — the model sees
every kind of state, what is authoritative, what is current, what is
missing and why; not every byte of history. The manifest is the
audit record of that census. Flipping the model-facing input to the
structured frame requires the Frame-3 live non-inferiority gate and a
separate acceptance — measurement never drifts into behavior.

## Reading the data

```bash
# capture: run with the flag on (compose flag shadow_context_frame)
agent-replay --frame-report <trace.jsonl>   # cost/coverage comparison table
agent-replay --frame-gate <trace.jsonl>     # CI gate; non-zero on violation
```

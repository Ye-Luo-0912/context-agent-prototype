# Execution model

The state machine one run moves through. This is the operator-facing
summary of the actor machinery (`agent-runtime::actor`); the durable
contract lives in [`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md) and
the invariants in [`AGENTS.md`](../AGENTS.md).

## Objects

| Object | Lifetime | Owner |
| --- | --- | --- |
| **Run** | process start → stop | the actor; identity is the `RunId` on every event envelope |
| **Task** | created → completed (long-lived, survives checkpoints) | `TaskManager`; authority is the `TaskAnchor` (goal, constraints, acceptance, plan, open loops, `next_action` advisory) |
| **Turn** | user input → `TurnCompleted` + commit barrier | the actor's `TurnFrame` (action batch, round surface, protocol bodies, repair episode) |
| **Operation** | one model call or one tool call inside a turn | tracked `InFlightOp` (generation-fenced, cancellable); tool ops carry Core operation identity and a staged effect |
| **Effect** | prepared → dispatched → acknowledged | brokered through Core; unacknowledged applications become typed debts |
| **Checkpoint** | safe-point → durable artifact | `CheckpointStore` envelope (see [`COMPATIBILITY.md`](COMPATIBILITY.md)) |

## Turn lifecycle

```text
user input accepted
  → context maintenance (UserInput) → materialize → assemble → model round
  → model returns text and/or tool calls
      → tool calls: approve → reserve → dispatch → settle → observe
      → a task.complete call routes through the completion gate:
          ready     → accept → pending terminal commit → terminal transaction
          not ready → typed refusal → semantic repair episode (bounded;
                      cycling ends in an audited text-only handoff)
  → TurnCompleted + RuntimeCommitBarrier (one durable batch)
  → next model round, or finalize_turn
```

Safe-point checkpoints drain accrued debt (anchor change, durable
mutation, verification change, repair stage, opportunity offer, failed
terminal commit) as one background atomic write; the debt ledger is
retired only by the durable acknowledgement.

## Completion liveness

Completion is gated, not granted: a proposal is accepted only when the
typed blocker frontier allows it and trusted verification is current.
The repair episode persists across anchor/world churn, counts refusals
per blocker fingerprint, and only a strictly lower blocker potential
counts as progress. Automatic host proof refresh (opt-in deferred — see
[`PROOF-OPERATION` history in the review](reviews/2026-09-05-code-review.md))
runs the exact recipe for a sole-blocker refusal; a PASS must agree with
dispatcher attribution or it records a negative lease instead.

## Recovery

- Recovery fence: any unresolvable ambiguity (late stale effect,
  preparation cleanup failure, ambiguous ack) blocks further mutation
  until reconciled.
- Ack debts travel in checkpoints; a restored runtime re-fences until
  each debt reconciles against the broker journal (Applied/NotApplied
  clears, Ambiguous keeps the fence).
- Cold resume: `--restore` validates the envelope and payload before any
  mutation; the committed prefix replays, uncommitted suffixes never do.
  Operator procedures: [`RECOVERY_RUNBOOK.md`](RECOVERY_RUNBOOK.md).

## Durability ordering (the parts correctness rests on)

- Events and their commit barrier share one durable flush; replay keys
  off the barrier, so subscribers never see what replay cannot.
- Effects commit only after the generation fence revalidates; a stale
  operation's staged effect is rolled back, never applied.
- Input consumption/archival is accounted inside the same batch as the
  commit marker it belongs to.
- The event journal is append-only; the sequence gap check plus the
  barrier cursor is what makes recovery trustworthy
  (`agent-replay --recover`).

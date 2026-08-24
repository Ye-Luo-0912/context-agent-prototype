# One-shot terminal completion diagnostic (2026-08-24)

## Decision

An accepted `task.complete` now ends the turn at the existing durable safe
point once every sibling action has settled successfully and the current
verification gate still passes. A failed sibling or invalidated verification
continues to another model decision. Both live arms passed hidden 4/4, and the
event trace proves each clean completion batch is followed directly by
`AssistantMessage` / `TurnCompleted` / `TaskCompleted`, not another
`ModelStarted`.

| Metric | Dynamic C | Append A |
| --- | ---: | ---: |
| Hidden assertions | 4/4 | 4/4 |
| Model rounds | 74 | 47 |
| Tool calls | 92 | 43 |
| Failed tool outputs | 3 | 1 |
| Max rounds in one turn | 10 | 5 |
| Model input tokens | 289,817 | 327,860 |
| Selected resident/reactivated tokens | 8,332 | 101,691 |
| Historical-context prompt tokens | 46,562 | 145,768 |
| Final resident bytes | 471 | 22,895 |
| Successful task completions | 9 | 3 |

The change removes a semantically redundant confirmation call and preserves
the failure-recovery path, but live model variance obscures a whole-run round
reduction. More importantly, the C/A call gap remained 49 while failures were
only 2 apart.

## Root cause exposed by per-turn replay

Dynamic C closed the durable task on 9 of 15 user turns; A did so on 3. Every
closure made the next directive start with a new task id and empty
task-scoped `TaskProgress`. C then re-ran capability discovery, directory
listing, reads, and searches. The trajectory is a feedback loop:

`substep success -> task.complete -> task affinity cleared -> rediscovery -> task.complete`

This is not a Context-size defect. C still used 92% fewer selected tokens and
98% fewer final resident bytes. It is a lifecycle/surface defect: ordinary
turn completion and durable multi-turn task closure were exposed as if they
were the same operation.

This dirty-tree, `n=1` pair is diagnostic evidence, not M15 acceptance.

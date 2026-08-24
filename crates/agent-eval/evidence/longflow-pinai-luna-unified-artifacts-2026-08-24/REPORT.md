# Unified artifact/completion contract diagnostic (2026-08-24)

## Decision

Both direct PinAI/Luna Responses arms passed all four hidden assertions. The
retained tool-contract fixes reduced Dynamic C failed outputs from the earlier
29 to 2 without changing Context selection, GC, retrieval budgets, or packing.
They did not close execution amplification: C still used 77 model rounds and
84 tool calls versus A's 49 and 36.

| Metric | Dynamic C | Append A |
| --- | ---: | ---: |
| Hidden assertions | 4/4 | 4/4 |
| Model rounds | 77 | 49 |
| Tool calls | 84 | 36 |
| Failed tool outputs | 2 | 1 |
| Max rounds in one turn | 9 | 6 |
| Model input tokens | 289,344 | 315,964 |
| Selected resident/reactivated tokens | 9,512 | 99,170 |
| Historical-context prompt tokens | 43,867 | 140,639 |
| Final resident bytes | 998 | 20,614 |

The one model-visible spill reader (`artifact.read`), operation-aware
`context.manage` parsing, and Runtime-owned completion evidence therefore fix
real reliability defects. C retained an approximately 90% selected-context
advantage and 95% resident-byte advantage, but failures were no longer large
enough to explain the 48-call C/A gap.

## Residual cause

C produced 68 evidence-only results versus A's 21 while outcome advances were
16 versus 15. Successful `task.complete` calls were 10 versus 6. At this point
each accepted completion still caused another model confirmation round, and
each committed completion closed the task, clearing task-scoped progress for
the following user directive. The next experiment therefore separated
terminal confirmation cost from durable task-boundary cost rather than
retuning Context.

This dirty-tree, `n=1` pair is diagnostic evidence, not M15 acceptance or a
success-rate estimate.

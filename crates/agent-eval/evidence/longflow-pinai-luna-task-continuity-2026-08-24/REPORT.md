# Intent-gated task continuity diagnostic (2026-08-24)

## Decision

The lifecycle hypothesis is strongly supported, but the success-neutral gate
remains open. `task.complete` was moved from the ordinary always-loaded surface
to the bounded catalog. Runtime leases it for explicit task-closure intent or
an explicit task requirement; the model can also discover and load it through
`capability.manage`. Ordinary final output ends a turn without closing the
multi-turn task.

Across two independent direct PinAI/Luna Responses pairs, C no longer closed
the task during the 15-turn trajectory. Its median rounds/calls fell to 53/48,
near A's 48.5/41.5 and far below the immediately preceding C runs at 74–77
rounds and 84–92 calls. Context remained substantially lighter.

| Pair | Arm | Hidden | Rounds | Calls | Failed | Max turn | Model input | Selected tokens | Final resident bytes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| r1 | Dynamic C | 3/4 | 49 | 44 | 1 | 7 | 194,296 | 22,179 | 4,373 |
| r1 | Append A | 4/4 | 50 | 45 | 2 | 7 | 322,503 | 107,724 | 21,488 |
| r2 | Dynamic C | 4/4 | 57 | 52 | 2 | 8 | 265,681 | 25,799 | 4,181 |
| r2 | Append A | 4/4 | 47 | 38 | 0 | 5 | 279,611 | 83,694 | 18,375 |

Median C model input was about 24% below A, selected tokens about 76% below,
and final resident bytes about 80% below. Evidence-only results were 28/32 for
C versus 27/22 for A: the former 74-result C evidence loop disappeared rather
than being hidden as successful tool output.

## The one failed assertion

C r1 did execute the requested `RELEASE.md` update. `edit.patch` committed a
291 -> 373 byte change and its result echo showed the new line. The model wrote
`Version 2 Hello support ... v:2:...` where the hidden checker required the
literal lowercase substring `v2`. C r2 wrote an accepted form and passed 4/4.
This is a real instruction-fidelity miss even though it is not a missing edit,
filesystem failure, or task-continuity loss.

Therefore the algorithm is retained as a working-tree candidate with strong
convergence evidence, not declared accepted: C passed 1/2 pairs while A passed
2/2. Do not close the success-neutral gate, retune Context, or claim M15 from
these dirty-tree diagnostics. A broader clean repeat set must show unchanged
hidden success and no new max-turn tail.

The independent second pair is stored under
`../longflow-pinai-luna-task-continuity-r2-2026-08-24/`.

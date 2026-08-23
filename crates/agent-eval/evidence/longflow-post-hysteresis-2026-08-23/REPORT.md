
## A/C round-gap attribution (dynamic 94 vs append 53)

- process.run guessing loop: 20 failed calls = 18 program_not_found
  (never-compiled test binaries tried as protocol_tests.exe / ./ / .\
  / _new / heartbeat_ variants despite the listing-and-build hint) +
  2 real compile errors (fixed). Largest single source, ~20 rounds.
- identity-only rereads: proto=8 + desc=6 + reval=8 fs.read motives.
- git status/diff re-verification repeats (+18): dynamic forgets old
  outputs by design, append keeps them resident.
- remainder: normal exploration spread.

## Retry-reduction plan

1. Harden the failure-cluster line for consecutive program_not_found:
   include the already-tried names and an explicit build-first
   instruction (advisory today fired but was too weak - 18 attempts).
2. Fail-fast variant of the cluster: after K same-class refusals over an
   unchanged world, refuse further never-seen program names in that cwd
   with the listing + build advice attached (refusal, not execution).
3. Optional task-anchor requirement: when the directive asks to run
   tests and no built artifact exists yet, surface a build-first need
   through the existing task tool-demand channel.

## Self-contained comparison (this evidence dir, r1)

| metric | dynamic (C) | append (A) |
| --- | --- | --- |
| outcome | passed | passed |
| rounds | 94 | 53 |
| tool calls | 135 | 52 |
| model input tokens | 849850 | 605005 |
| input/round | ~9.0K | ~11.4K |
| model output tokens | 19076 | 9618 |
| schema tokens | 96605 | 48761 |
| capability.manage | 7 (6 first loads, 0 reloads, 0 failed) | 4 |
| fs.read | 25 (1 failed) | 18 (2 failed) |
| process.run | 35 (20 failed = 18 never-compiled guesses) | 4 (2 failed) |
| edit.replace / git.status / git.diff | 14 / 11 / 13 | 9 / 4 / 2 |

Historical-context cost per round is the structural win of Dynamic
Context: roughly 1.3K tokens/round on C vs 3.6K on A (~1/3), while total
rounds stayed trajectory-dominated (n=1 live runs; do not read 94 vs 84
across dirs as regression - different trajectories, one also flagged
provider token accounting as a lower bound).

fs.read motives (C): proto-checkpoint-missing=8, descriptor-only=6,
needs-revalidation=8, warm=0, stored=0. Resident/forgotten details live
in dynamic/summary.json alongside hidden-PASS verification records.

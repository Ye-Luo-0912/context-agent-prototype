
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

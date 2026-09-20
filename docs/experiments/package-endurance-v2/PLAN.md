# Package repository and transactional deployment endurance v2

Status: NOT_ACCEPTED_DECISION_BUDGET_EXHAUSTED (2026-09-20).
The original 240-decision limit was reached; application gates failed and the
90-minute load was not started. See [the run receipt](RUN_2026-09-20.md).
The plan below records the intended scope, not completed coverage.

This campaign evaluates one real model improving an isolated Python package
repository/deployer, plus deterministic public-host fault journeys against the
same Runtime binaries. The deployer handles 120 package names, four versions
each, dependency backtracking, content-addressed package bytes, concurrent
deployment, idempotent receipts, interrupted commits, migration and GC.

Eight stages: baseline; dependency resolution; cache/content integrity;
concurrent deployment and cancellation; publication/receipt reconciliation;
migration and crash recovery; public-host T1/T2/T1 interleaving; acceptance.
The model implements application features. The controller, not the model,
injects process death, bad responses, wrong TaskId, cancellation and connection
loss. Public-host actions and synthetic responses are recorded separately from
provider-paid attempts. No synthetic result is counted as autonomous work.

Target 2–3 hours including 90 minutes of deployment load; hard campaign limit
4 hours. At most 240 main decision requests, 720 tool attempts, 300 total
provider attempts, 9M input and 300k output tokens. Shared estimated cost cap
USD 3 using the verified DeepSeek Flash peak rates (input 0.30/M, cached input
0.006/M, output 1.20/M), not an account billing guarantee. Unknown usage retains
its reservation and stops new paid calls. Retries/restores consume the same
campaign budget; there is no per-segment reset. Each request reserves its wire
byte count as an input-token upper estimate plus full output cap. One paid
request at a time; maintenance is disabled in the paid implementation lane and
tested separately with real ContextEngine/compactor local fixtures.

The independent oracle is outside the model workspace and never imports app
implementation. It reads actual lockfiles, pointers, package blobs, database
receipts and receiver audit; it cannot replace a disk check with re-execution of
the candidate. Calibration must reject a corrupted blob, altered pointer and a
missing receipt before real acceptance runs. File hashing detects mutation;
directory separation is not a claim of OS access isolation.

Required observed boundaries: new directive after safe provider failure and cold
restore; old GC plus checkpoint prepare and model failure; bounded status/cancel;
partial tool batch cancellation; wrong-task steering; T1/T2/T1 with distinct
actual TaskIds; lost publication response with query-before-retry; expired
permission; stale file edit; actual cold-body consumption and compaction;
spoofed verification text never grants completion; exact known usage settlement.
Existing focused tests may supply component evidence but cannot stand in for a
new combination that was never triggered. Each case gets fired/observed/verdict.

Ordinary application defects are recorded and repaired within the fixed budget,
or the assisted branch is labeled. Authority breach, blind replay of an unknown
effect, cross-task state adoption or false durability stops the side-effectful
branch and preserves its evidence. All unrelated coverage continues. On a
blocking Runtime integrity failure, report the exact reproduction instead of
claiming the campaign complete. Production edits and Git publication are not
part of this test run unless needed and explicitly reported.

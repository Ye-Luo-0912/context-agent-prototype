# Full long-flow campaign receipt

Status: **COMPLETE_WITH_MANUAL_REPAIR**. P1–P8 real-model stages and the
declared 90-minute P3 soak finished on one TaskId; deterministic app and Runtime
gates are recorded below.

Machine-readable aggregate receipt: `target/runtime-endurance-v1/incremental-platform-20260919/full-campaign-receipt.json`.

## Real model stages

| Stage | Rounds | Tools | Result |
| --- | ---: | ---: | --- |
| P1/P2 baseline, DAG, CAS, fingerprints | 24 | 59 | `turn_completed` |
| P3 workers and bounded-load implementation | 24 | 70 | `turn_completed` |
| P4 outbox, receiver and uncertain response | 24 | 48 | `turn_completed` |
| P5 v1→v2 migration and tenant/destination scope | 24 | 56 | `turn_completed` |
| P6 incident correction and long directive | 24 | 57 | `turn_completed` |
| P7 T1/T2 isolation and return | 12 | 29 | `turn_completed` |
| P8 final delivery and acceptance | 24 | 63 | `turn_completed` |

The initial malformed runner attempt made zero provider calls and is excluded
from the model budget. The seven successful segments used 156 model requests,
382 tool attempts and a peak token-cost estimate of about `$1.04`; every segment
kept the protected fixture hashes unchanged. The current request/task state is
still `awaiting_operator_review`; ordinary final is not operator closure.

## Deterministic gates already completed

- v1 fixture tests: 2/2.
- P3 smoke: 12 batches / 120 records, four worker processes, oracle match.
- P3 crash recovery: real child claim, hard kill, lease expiry, new token and
  successful reclaimer; old token never acknowledged.
- CAS/fingerprint verifier: content change invalidates the fingerprint, nested
  live identities remain reachable during GC, manifests remain immutable.
- Migration verifier: genuine interrupted v1 state resumes to v2 and is
  idempotent on a second call.
- Outbox verifier: same identity/body conflict is rejected, non-localhost is
  refused, receipt reconciliation confirms a sent item.
- Real localhost receiver: first response was dropped, second attempt reused
  the same idempotency key, receipt query settled the original identity.

## Terminal soak gate

The independent soak controller owned the load producer and four real
application workers. It enforced 2 batches/second, 10 records/batch, a 5,400
second deadline and a hard 108,000-record cap. The terminal receipt shows all
jobs succeeded, no queued/running rows, no mismatches and no missing manifests.

The terminal receipt is `target/runtime-endurance-v1/incremental-platform-20260919/soak-receipt.json`:

- 5,400 seconds, 10,800 batches and 108,000 records;
- 10,800 committed jobs, all with receipts and no queued/running residue;
- oracle mismatches 0, failures 0, missing receipts 0;
- final plan digest `21efaf5fba28fe61349223cc194a35d9b22f352d760928682c91bb0081820076`.

The controller repaired the Windows spawn-safe worker entry and CAS nested live
identity enumeration after the model stages; those changes are labeled manual
controller repairs in the final RESULT and are not autonomous model credit.

The short P3 smoke controller itself left one Windows multiprocessing child
after writing its receipt; that controller-owned PID tree was explicitly
terminated during cleanup. The 90-minute soak controller reached its own idle
join and left no process residue.

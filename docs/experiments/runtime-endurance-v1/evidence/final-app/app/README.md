# Incremental Build and Publication Platform

This workspace is the v1-to-v2 endurance fixture. It imports bounded JSONL
records, evaluates a deterministic DAG, stores immutable CAS blobs/manifests,
claims build jobs with leases, publishes through an idempotent outbox, and
migrates its SQLite metadata from v1 to v2.

Run a local build with:

```powershell
python -m app.cli --input fixtures/input.jsonl --plan fixtures/plan.json --output result.json
```

Worker processes use `app.queue.JobQueue` and `app.workers.WorkerPool`. The
load producer is independent (`app.load_driver`) and never claims jobs. A
worker stores its output in `app.store.CasStore`, publishes a manifest, and
acknowledges only with its current lease token. A stale worker can therefore
finish computation but cannot overwrite the newer owner.

The outbox binds a stable identity to one body and retries with the same key.
`app.outbox.Outbox` refuses non-localhost targets. A missing HTTP response is an
uncertain result; callers query the receiver and settle the original identity.
This is at-least-once delivery plus receiver idempotency, not arbitrary external
exactly-once effects.

The isolated campaign recorded P1–P8 model segments, a 12-batch worker/load
check, a real crashed-worker recovery, the independent oracle, and the final
CAS/outbox/migration verifier. The long 90-minute soak is recorded separately
by the controller and is not represented by a short smoke test.

# Design and recovery boundaries

## Identity and fingerprints

`TaskId`/`RunId` belong to Runtime. Application identities are separate:
`AppJobId`, `AppRunId`, `ManifestIdentity`, `tenant`, `destination` and the
outbox idempotency key. A content fingerprint hashes canonical record bytes and
schema id. A node fingerprint hashes operator, canonical parameters, ordered
upstream fingerprints and schema. Changing bytes with the same length or mtime
changes the content fingerprint and invalidates affected descendants only.

## DAG and CAS

`app.engine.compile_plan` validates node IDs, dependencies and cycles before
execution. `reference_execute` is intentionally separate from the production
operator path and is used by the final verifier. CAS writes use temp + fsync +
atomic replace. Manifests are immutable; live pointers are atomic files. Live
identities may contain tenant/destination path components, so GC recursively
enumerates the live tree. GC deletes only blobs outside the reachable manifest
closure.

## Workers and leases

Workers are real OS processes. SQLite `BEGIN IMMEDIATE` is the queue authority.
Claim increments attempts and mints a token; ack/heartbeat/fail require the
current token and an unexpired lease. A crashed worker leaves a running row;
the next claim reaps it and receives a new token. The old token is rejected.
The spawn entry point receives only serializable paths, primitives and
multiprocessing handles; the supervisor's live Process objects are never
pickled into a child.

## Publication and migration

The outbox is local durable state. It records `pending → inflight → sent` and
receiver confirmation separately. The same identity and body are reused after a
timeout. A conflicting body under the same identity is rejected. Application
HTTP receipts do not stand in for Core prepared/apply/ACK facts.

Migration journals each v2 step before and after its transaction. A restart
leaves `user_version=1` until all steps commit; idempotent steps resume after an
interruption. v2 adds publication/outbox/receipt/index structures without
dropping v1 rows.

## Known limits

Payload execution is a fixed operator registry; arbitrary payload code is never
executed. The fixture receiver is localhost-only. This fixture demonstrates the
Runtime and application boundaries but does not claim arbitrary external
exactly-once semantics or infinite history bounds.

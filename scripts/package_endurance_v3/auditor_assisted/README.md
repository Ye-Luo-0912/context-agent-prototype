# Assisted read-only auditor

This is a separately repaired copy of the real-model authored `app/audit.py`
from the bounded package-endurance-v3 audit task. It is **ASSISTED**, not a new
model result. The original model output, its failing tests, the application
fixture, and the load source remain unchanged. The model supplied the initial
CLI, digest checks, receipt enumeration, sidecar refusal, and immutable SQLite
opening. Manual changes below address independently reproduced false passes.

Run `python -B -m app.audit --root SNAPSHOT` from this directory. The result is
one JSON document and exit 0 for PASS or 1 for detected corruption/incomplete
inspection. The input must be a quiescent package repository snapshot with the
v2 `receipts` and `current` authority tables. Any SQLite WAL, SHM or rollback
sidecar is refused; the auditor never repairs the snapshot or checkpoints it.

The assisted copy binds manifest tenant/environment to each receipt, validates
scope types and sorted package entries, and checks every active pointer against
SQLite `current` as well as its historical receipt. A valid old receipt cannot
justify a stale current pointer. Missing current authority/current pointers and
invalid deployment directory names produce explicit errors. Strict JSON rejects
duplicate keys and non-finite values, and pointer bytes must be canonical.

Every path component under the snapshot root is checked with `lstat`; symlinks
and Windows reparse points (including junctions) are refused before content is
read. SQLite URIs percent-encode the path, including `#`; the original unescaped
URI was observed creating a new database outside a `snapshot#one` root because
the URI fragment swallowed its read-only query parameters. This is a quiescent
snapshot auditor, not a race-proof filesystem sandbox: an actively mutating
writer invalidates the input precondition. Unsupported schema, unreadable roots,
or malformed evidence must not be promoted to a successful audit.

Independent tests construct actual disk/SQLite states without importing auditor
helpers, exercise the CLI, and check complete before/after byte hashes. The
Windows path-escape test uses a real directory junction. Model selftests contain
two historical-corruption cases whose fixture omits the claimed historical DB
row; those model tests are preserved, not silently repaired or counted as proof.

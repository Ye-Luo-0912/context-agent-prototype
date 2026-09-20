# V5 Online backup and safe continuation contract (frozen 2026-09-21)

Implement `app.live_backup` with Python standard library only. The existing
repository install, recovery, GC, and publication APIs are the application
under test and must keep their prior behavior. Only `app/live_backup.py` and
`app/tests/test_live_backup.py` may be created or changed by the model.
The controller, this contract, fixtures, and public tests are protected.

The public functions are:

```python
backup_live(root, archive, *, crash_at=None) -> dict
restore_live(archive, destination, *, crash_at=None) -> dict
```

Both arguments accept strings or `pathlib.Path`. A successful call returns one
JSON-compatible summary with exactly `cut_id`, `scopes`, `receipts`,
`manifests`, `objects`, and `outbox`. Invalid or incomplete state raises
`ValueError` or `OSError`; it must never return a partial success.

## Reading mode and the cut window

The controller keeps four install workers, a reader/GC worker, and a loopback
publication receiver running against the same repository. The source is
**quiesced for the duration of one `backup_live`/`restore_live` call** and
writers resume afterwards; the controller records a source fingerprint
immediately before and immediately after every call, and a batch whose interval
changed is refused instead of being counted as a candidate failure.

Inside that interval one cut is defined as:

* one explicit SQLite read transaction that covers every authority read
  (`receipts`, `current`, `journal`, `outbox`, `PRAGMA user_version`);
* the referenced manifest/object/pointer bytes read inside the same
  transaction window and verified against their hashes.

The database must be opened through a correctly percent-encoded **read-only**
URI (`?mode=ro`). `mode=ro` is the frozen mode because it includes the
committed WAL that belongs to the authority while still refusing every write.
`immutable=1` must not be used: SQLite defines it as a promise that the file
cannot change, which is not true for a repository that is being written around
the call, and it silently hides a committed WAL. The reader must not create a
WAL or a rollback journal, must not checkpoint, and must not modify the
database or any pointer file. Attaching to an existing committed `-wal` (and
its `-shm`) is allowed.

A file or row that changes under a concurrent writer must produce a bounded
`ValueError`/`OSError` refusal, not a partial success. Such a refusal is
evidence, not a defect of this contract.

Files that exist under `root` but are not part of the referenced closure are
neither required nor rejected: the archive never encodes them, and `root` is
never mutated by a backup.

## Authority invariants

* SQLite `user_version` is 2 and the tables `receipts`, `current`, `journal`,
  and `outbox` exist.
* Every scope has contiguous historical generations starting at 1, one `current`
  row naming the **maximal** generation, and a matching canonical
  `deployments/T/E/current.json`. A gap, a duplicated current row, a current
  row without a receipt, or a current row below the maximal generation is
  rejected.
* Every receipt's manifest is present, canonical, hash-valid, and written for
  the same `(tenant, environment)` as the receipt. A cross-scope manifest is
  rejected.
* Every manifest's objects are present, canonical, and hash-valid.
* Duplicate receipt, journal, or outbox identities are rejected. Historical
  receipts are all retained.
* A journal row is part of the cut only with its resources: its
  `old_manifest`/`new_manifest` and the manifest named by its new receipt must
  all be present.
* A non-empty journal or outbox is part of the cut and must be represented in
  the archive. It cannot be dropped to make the snapshot look clean. An
  implementation may refuse a cut when it cannot encode those obligations.

## Archive

The archive is deterministic ZIP_STORED with lexical member order and
canonical UTF-8 JSON. It contains `descriptor.json`, all receipt rows,
manifests, objects, canonical `deployments/T/E/current.json` pointers, one
`journal/` member per journal row, and one `outbox/` member per pending
publication. There are no timestamps, source paths, host names, duplicate
entries, compression, links, absolute paths, or ZIP64.

Frozen bounds — these exact numbers are the ones the controller enforces and
the ones the readiness check compares against this text:

```
member count           1024
single member bytes    2 MiB
uncompressed total     8 MiB
archive bytes          9 MiB
```

The controller computes the maximum referenced closure of the frozen workload
(member count, uncompressed bytes, archive bytes) before any window opens and
refuses to open one when the closure would exceed these bounds. The model must
not widen a bound, and a candidate may not cap historical receipts to satisfy
one.

Write to a sibling temporary file, then publish atomically without overwriting
an unrelated archive. A conflicting existing archive is rejected unchanged.

`crash_at='before_publish'` must exit the child process with code 74 after the
complete archive has been closed but before the destination archive rename.
The retry must succeed and may only clean its own uniquely named temporary file.

## Restore

Validate the complete archive before writing. Reconstruct a v3 repository in a
unique sibling staging directory on the destination filesystem, including the
SQLite authority, all referenced files, current pointers, and journal/outbox
rows. A single rename publishes an absent destination.

An existing destination succeeds only when it is byte-for-byte the cut: the
same authority, the same member bytes, and no additional file. Its only
permitted additional entries are `repo.sqlite` and the SQLite sidecars of a
database opened in WAL mode. An empty, partial, linked, extra-file, or
mismatching destination is rejected unchanged.

`crash_at='before_publish'` must exit code 74 after a complete staging
directory has been closed. The destination remains absent, the abandoned stage
remains untouched, and a later retry succeeds without adopting unknown stages.

## Continuous behavior

Each batch records the observed request identity, current generation, cut
window fingerprint, cut ID, archive hash, restore hash, and receiver
publication audit. The model must inspect `runtime-feedback/latest.json`
between repairs; unresolved failures stay visible there until the same
obligation is proven resolved. A failed batch is evidence and does not erase
previous receipts, and it does not prevent a later frozen candidate version
from opening its own acceptance window. The controller never imports candidate
helpers to define expected answers.

No network or package payload execution is allowed. The model must not inspect
controller/oracle source or Runtime private state. The independent oracle reads
actual SQLite, archive bytes, restored files, and receiver logs. It rejects
cross-tenant data, dropped outbox obligations, stale current pointers, replayed
publication identities, and any source mutation.

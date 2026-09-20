# V5 Online backup and safe continuation contract

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

## Online cut

The source is a quiescent v3 repository while the controller may perform
install, read, GC, and publication operations around the call. A valid cut is
one coherent authority point, not a copy of only `repo.sqlite`:

* SQLite user_version is 2 and the tables `receipts`, `current`, `journal`, and
  `outbox` exist. The read must use a correctly percent-encoded immutable
  read-only URI and must not create WAL, SHM, or journal sidecars.
* Every scope has contiguous historical generations, one current row naming the
  maximal generation, and a matching canonical `deployments/T/E/current.json`.
* Every referenced manifest and object is present, canonical, and hash-valid.
  Historical receipts are all retained. Duplicate identities, cross-scope
  manifests, missing data, stale pointers, and unknown files are rejected.
* A non-empty journal or outbox is part of the cut and must be represented in
  the archive. It cannot be dropped to make the snapshot look clean. An
  implementation may refuse a cut when it cannot encode those obligations.

The archive is deterministic ZIP_STORED with lexical member order and
canonical UTF-8 JSON. It contains `descriptor.json`, all receipt rows,
manifests, objects, canonical `deployments/T/E/current.json` pointers, and an
`outbox/` member for each pending publication.
There are no timestamps, source paths, host names, duplicate entries,
compression, links, absolute paths, or ZIP64. Bound members at 256, each member
at 2 MiB, total uncompressed members at 8 MiB, and archive bytes at 9 MiB.
Write to a sibling temporary file, then publish atomically without overwriting
an unrelated archive. A conflicting existing archive is rejected unchanged.

`crash_at='before_publish'` must exit the child process with code 74 after the
complete archive has been closed but before the destination archive rename.
The retry must succeed and may only clean its own uniquely named temporary file.

## Restore

Validate the complete archive before writing. Reconstruct a v3 repository in a
unique sibling staging directory on the destination filesystem, including the
SQLite authority, all referenced files, current pointers, and outbox rows. A
single rename publishes an absent destination. An existing destination succeeds
only when its complete byte map and read-only authority exactly match the cut;
an empty, partial, linked, or extra-file destination is rejected unchanged.

`crash_at='before_publish'` must exit code 74 after a complete staging directory
has been closed. The destination remains absent, the abandoned stage remains
untouched, and a later retry succeeds without adopting unknown stages.

## Continuous behavior

The controller will run this API while four install workers, a reader/GC worker,
and a loopback publication receiver operate on the same repository. Each batch
records the observed request identity, current generation, cut ID, archive hash,
restore hash, and receiver publication audit. The model must inspect
`runtime-feedback/latest.json` between repairs. A failed batch is evidence and
does not erase previous receipts. The controller never imports candidate
helpers to define expected answers.

No network or package payload execution is allowed. The model must not inspect
controller/oracle source or Runtime private state. The independent oracle reads
actual SQLite, archive bytes, restored files, and receiver logs. It rejects
cross-tenant data, dropped outbox obligations, stale current pointers, replayed
publication identities, and any source mutation.

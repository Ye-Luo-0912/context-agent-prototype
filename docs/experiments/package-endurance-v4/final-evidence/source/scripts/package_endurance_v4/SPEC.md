# V4: deterministic snapshots and atomic restore

Implement `app/snapshot.py` using Python standard library only. The existing
assisted `app.repository.Repository` application is already accepted; preserve
it. Only `app/snapshot.py` and `app/tests/test_snapshot.py` may be created or
changed. SPEC.md, fixtures/, tests/, existing app files, and Runtime private
state are protected. Never execute package payloads or access the network.

## Public API and CLI

Provide these APIs; accept strings or pathlib paths and return the summary
described below. Invalid, unsafe, incomplete or conflicting input raises
`ValueError` or `OSError`, never a successful partial result.

```python
export_snapshot(root, archive) -> dict
inspect_snapshot(archive) -> dict
restore_snapshot(archive, destination, *, crash_at=None) -> dict
```

The CLI is `python -m app.snapshot export --root ROOT --archive ZIP`,
`python -m app.snapshot inspect --archive ZIP`, and
`python -m app.snapshot restore --archive ZIP --destination DESTINATION`.
Restore additionally accepts `--crash-at before_publish`. Success prints one
JSON object `{"ok":true,...summary fields...}` and exits 0. Failure prints one
JSON object `{"ok":false,"error":"..."}` and exits 1. Do not print tracebacks,
progress lines or a success object after a failed operation.

Every success summary has exactly `snapshot_id` (lowercase SHA256 of the
canonical snapshot.json bytes), `receipts`, `current`, `manifests`, `objects`
(integer counts, not booleans). All three APIs return the same summary for
the same snapshot; no timestamp, source path, randomness or host name is part
of its identity. An empty but valid repository is allowed.

## Supported source repository

Input is a quiescent v3 repository with SQLite user_version=2 and the four
tables from app/state.py: receipts, current, journal, outbox. Read its database
through a correctly percent-encoded read-only immutable SQLite URI. A path
containing `#`, `%`, spaces or Unicode remains a literal filesystem path.
Never open a writable connection, call Repository (which opens WAL), recover,
checkpoint, migrate or repair an export/inspect input.

Refuse any repo.sqlite-wal, repo.sqlite-shm or repo.sqlite-journal file; any
row in journal or outbox; and any entry in the optional disk journal directory.
Even acknowledged outbox rows are outside this snapshot format: do not erase
external-publication identity by silently omitting them. No concurrent writer
is supported. User-supplied parents are trusted, but source root and every
descendant inspected must be real directories/regular files; reject symlinks
and Windows reparse points before reading. No guarantee against hostile
concurrent replacement of a validated path is required.

Names (tenant, environment, key, package) match `[a-z][a-z0-9-]{0,63}`.
Digests are exactly 64 lowercase hexadecimal characters. Reject booleans as
integers. Every generation and package version is a positive integer. Within
each (tenant,environment), committed generations are unique and contiguous
1..N and current names generation N. All scopes with receipts have exactly
one current row and one deployments/TENANT/ENVIRONMENT/current.json; there
are no orphan or unrecognized deployment paths. Receipt identity is unique.

The receipts table exposes tenant, environment, key, generation,
manifest_sha256 and request_json. Preserve each original request_json string
exactly. It must itself be canonical JSON with exactly `requirements` and
`expected_generation`; requirements maps legal package names to exactly
`min` and `max` integers with 1<=min<max. expected_generation is null or a
nonnegative integer. Do not rerun dependency resolution during snapshotting.

The current table's receipt_json and each current.json are the canonical
five-field receipt {tenant,environment,key,generation,manifest_sha256}; each
must equal the matching historical receipt with request_json removed.
Historical receipts must not be compared to the latest pointer as if they
were all current. Extra unreferenced regular files under objects/ or
manifests/ may be ignored by export; their names still must be legal digest
paths and their contents need not enter the archive. Other unknown source
root files are rejected. Empty journal/ is allowed.

Every referenced manifest is UTF-8 canonical JSON exactly
{tenant,environment,packages:[{name,version,sha256},...]}, with unique package
names in ascending order and scope equal to every receipt referring to it.
Hash its real bytes and all referenced object bytes. Missing or corrupt
historical data is a failure even when current data is sound.

## Canonical archive format and bounds

Canonical JSON means UTF-8, ensure_ascii=False, sort_keys=True,
separators=(',',':'), allow_nan=False, no BOM and no trailing newline. Reject
duplicate JSON keys and NaN/Infinity. Canonical content must equal re-encoding.

The archive contains exactly:

* snapshot.json: an object with exactly `format` = `package-snapshot-v1`,
  `schema_version` = 2, `receipts` and `current`.
* receipts is an array of six-field receipt rows including request_json,
  sorted uniquely by (tenant,environment,key).
* current is an array of five-field receipts, sorted uniquely by
  (tenant,environment).
* manifests/<sha256>.json: precisely the manifests referenced by ALL receipts.
* objects/<sha256>: precisely the blobs referenced by those manifests.

No SQLite database is embedded. No ZIP directory entries, duplicate names,
extra members, absolute paths, backslashes, dot segments, links, encrypted
entries, ZIP64, archive/member comments or member extra fields are accepted.
Every entry uses ZIP_STORED, timestamp (1980,1,1,0,0,0), create_system=3 and
external_attr=(0o100644 << 16), and entries appear in lexical filename order.
Exporting the same semantic repository twice must produce identical ZIP bytes.
Check metadata before reading bodies and verify actual bytes/CRC/hashes.

Bound every operation: at most 256 members, at most 2 MiB per uncompressed
member, at most 8 MiB total member bytes and at most 9 MiB archive bytes.
The descriptor has at most 128 receipts, 64 current scopes and 128 package
entries per manifest. Reject over-limit data rather than truncating it or
claiming partial validity. Apply the same limits before export publication.

## Mutation and crash rules

Export and inspect never change source bytes or create SQLite sidecars, even
on invalid input. Export output must be outside the source tree and must not
be a link. Write a sibling temporary file then publish atomically. An existing
archive is accepted only if its complete bytes already equal the intended
valid archive; otherwise refuse and preserve it. Do not overwrite unrelated
files or destinations. Parent directories must already exist.

Restore validates the entire archive before publishing anything. Build a
unique sibling staging directory on the destination filesystem; reconstruct
the SQLite v2 receipts/current authority and empty journal/outbox tables,
all referenced blobs/manifests, and canonical current.json files. Close SQLite
and remove only your own temporary products through normal cleanup; no SQLite
sidecars may remain when publishing. Publish the completed directory with a
single rename into a destination that is still absent. Restored roots contain
only repo.sqlite, referenced objects/manifests, deployments, and an optional
empty journal directory. Do not add trust-by-marker files.

For an existing destination, fully verify its repository state and complete
referenced content. Return success without writing only when its canonical
descriptor and exact referenced file set match this snapshot; any nonmatching,
corrupt, incomplete, linked or extra-file destination is rejected unchanged.
An empty existing directory is a conflict. The source archive is never changed.

`crash_at='before_publish'` calls real `os._exit(73)` after a valid stage is
fully written/closed but before rename. The destination must remain absent.
Other crash_at values are invalid. A retry without the hook must succeed once;
do not adopt or delete unknown abandoned stages and do not treat their presence
as proof of a published restore. Caught failures clean only their own stage.
Atomic rename semantics are required, not physical-power-loss guarantees or
hostile concurrent writer safety. A mismatch never authorizes effect replay.

## Acceptance

Public smoke fixtures contain multiple tenants, environments, generations and
a literal `#` in the source path. They are only preliminary checks. Additional
independent controller tests construct real SQLite/files and malicious ZIPs,
compare complete before/after byte maps, exercise real exit 73, and inspect
restored authority without importing candidate helpers. Run your own tests;
report failures and limits honestly. Ordinary final is not operator acceptance.

# Package repository / transactional deployer v2

Upgrade the runnable v1 fixture, using Python standard library only. Application
code and documentation may change only under app/. SPEC.md, fixtures/ and tests/
are immutable. Never read or write Runtime private state or run package payloads.
Fixtures contain UTF-8 data packages only. No install/download/network except the
controller's explicitly supplied localhost publication receiver.

## Required APIs (app/repository.py)

`resolve(catalog, requirements) -> dict[str,int]`. Catalog is a dict from legal
package names to version entries, each {version:int, sha256:str, deps:dict}.
Each requirement is {min:int,max:int}, min inclusive/max exclusive, 1<=min<max.
Versions are positive ints, reject bool, duplicate versions/names/digests with
inconsistent content and unknown keys. Names match [a-z][a-z0-9-]{0,63}.
Select a version for every reachable package satisfying ALL constraints.
Backtrack when the locally highest version conflicts downstream. Among valid
solutions choose the lexicographically greatest version vector over sorted
reachable package names. Missing dependencies and unsatisfiable graphs raise
ValueError; reject cycles in the chosen solution. Never silently drop a root.
Return the sorted name->version mapping. Catalog/requirement inputs stay intact.

`Repository(root, catalog_path)` is a context manager with close(). Catalog JSON
is at catalog_path; immutable bytes live at catalog_path.parent/blobs/<sha256>.
The root contains repo.sqlite, objects/<sha256>, manifests/<digest>.json and
deployments/<tenant>/<environment>/current.json. Tenant/environment follow the
same legal-name rule. Every JSON output uses UTF-8, sort_keys=True, separators
(',', ':'), allow_nan=False, without a trailing newline for hashed content.

`install(tenant, environment, key, requirements, expected_generation=None,
crash_at=None) -> receipt`. Key follows legal-name rule. Validate before mutation.
Receipt fields: tenant, environment, key, generation, manifest_sha256.
Manifest fields exactly {tenant,environment,packages:[{name,version,sha256}...]},
packages sorted by name. Its sha256 binds its canonical encoded bytes. The
current.json file is exactly the canonical receipt. Generations start at 1 and
increase once per new successful install in that tenant/environment.

Idempotency identity is (tenant,environment,key), with the complete original
requirements/expected_generation bound to it. Exact replay returns the original
receipt and must not increment generation or re-activate an older deployment.
Different content under that identity raises ValueError atomically. Same key in
different tenants/environments is independent. A nonmatching expected_generation
raises ValueError, leaving the current pointer and committed receipts unchanged.
Bytes must be rehashed when read; same size/mtime is not proof of content. A
corrupted source/cache blob must be rejected or repaired from verified source,
never installed unchecked. No staging file may become a published manifest.

SQLite is the cross-process authority. Use transactions, not just Python locks.
The `receipts` table exposes tenant TEXT, environment TEXT, key TEXT, generation
INTEGER, manifest_sha256 TEXT, request_json TEXT; composite identity is unique.
It must retain original receipts across upgrades and ordinary restarts.
Use a durable journal to reconcile a crash between disk pointer and DB commit.
Supported test hook crash_at='before_pointer' or 'after_pointer' calls os._exit(71)
at that exact boundary. No fake exceptions. `recover()` reconciles the journal
before another mutation; exposes a consistent committed old/new state. Reissuing
the exact interrupted install after recover must finish it once. Never infer
that an unknown external publication did nothing.

`active(tenant,environment)` reads/validates the real pointer, returns receipt or
None. `receipt(tenant,environment,key)` reads the original durable receipt or
None. `gc()` retains every blob/manifest referenced by committed receipts or
unsettled journals; a missing/unreadable root defers deletion. New install and
GC must not race a referenced blob into deletion. Never follow paths outside
root, including malformed package/tenant names. Package payloads are data.

`migrate(crash_at=None)` upgrades an existing v1 repo.sqlite:
PRAGMA user_version=1; receipts(key TEXT PRIMARY KEY,generation INTEGER NOT NULL,
manifest_sha256 TEXT NOT NULL,request_json TEXT NOT NULL).
Legacy rows belong to tenant='legacy', environment='default'. Preserve exact
generation/digest/request values; add scope without aliasing. New DB version=2.
At crash_at='migration_after_copy' call os._exit(72) after copy but before the
version transition is committed. Reopen and recover without loss or partial
success. Reject unknown nonzero future versions without changing data.

`publish(tenant,environment,key,url) -> dict`. Persist original publication
identity/body before sending. Receiver protocol: POST url+'publish' with JSON
receipt body; GET url+'receipt?tenant=...&environment=...&key=...' returns 200
matching receipt, 404 absent. After a transport failure first query the same
identity. Only a definite 404 permits the same-body/same-key retry. 409 conflict
fails. Store returned receiver acknowledgement separately from install receipt.
Never follow redirects outside loopback or manufacture new keys after timeout.

## Delivery

Implement app/worker.py accepting --root --catalog --jobs --result. Jobs is a
JSONL file of install requests; process sequentially with per-job outcome
{request,ok,receipt|error}, written to result JSONL. Each worker must be an actual
independent process with its own Repository connection. Four workers may share
one root. A cancelled/failed process must not claim later jobs succeeded.

Document API/CLI, transaction and crash boundaries, migration, recovery and
limits in app/README.md, DESIGN.md, RECOVERY.md and RESULT.json. Add meaningful
tests under app/tests. Tests passing on old source are not current evidence.
The controller has additional independent acceptance. Report failures honestly;
ordinary final does not mean persistent task closure or operator acceptance.

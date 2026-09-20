# Assisted application baseline

This is a copied and repaired application fixture derived from the frozen
`package-endurance-v2/evidence/final-app`. These changes were made by the coding
assistant, not by a new experimental model run. The original v2 evidence is
unchanged. Passing this fixture does not repair or reclassify the v2 model result.

The public API remains `app.repository.Repository` and `resolve`. Run workers
with `python -m app.worker --root ROOT --catalog CATALOG --jobs INPUT.jsonl
--result OUTPUT.jsonl`. Every result echoes the full input request; its receipt
is the historical result, which may differ from the current deployment.

The installation transaction holds SQLite `BEGIN IMMEDIATE` through preparing
objects, writing the durable recovery journal, replacing the pointer, and
committing the receipt/current tables. After process death, the disk journal
provides the original request and predecessor while SQLite serializes recovery.
`before_pointer` and `after_pointer` still exit the real process with code 71.
Recovering an after-pointer crash commits that install once. A before-pointer
crash retains the old deployment. New mutations settle recovery first. Exact
historical replay returns the original receipt and only heals the authoritative
SQLite current pointer. GC holds the same write lock and defers when any root
cannot be read; it cannot delete an install's in-flight objects.

Publication persists the original receipt body and normalized loopback HTTP URL
before contacting the receiver. Every pending operation starts with GET of the
same identity, including the first attempt. Only HTTP 404 permits POST. A crash
or invalid acknowledgement leaves that intent pending. An acknowledged replay
returns the validated stored acknowledgement; different URL/body is rejected.
Responses must be the exact receipt, redirects are refused, and requests bypass
ambient proxies. SQLite serializes publication across processes while bounded
network requests execute. Thus a slow receiver can hold the global write lock
for up to multiple 10-second request timeouts; this baseline prioritizes bounded
correctness, not high throughput or per-tenant concurrency.

Tests use real process exits, independent SQLite/disk reads, and a localhost
receiver. They provide local fault-boundary evidence. They do not prove behavior
under OS/filesystem corruption, malicious database edits, physical power loss,
or arbitrary external servers. Parent directories supplied by the controller
must be trusted (this fixture does not provide a symlink sandbox). Resolver and
legacy migration remain inherited code except directly necessary validation;
they require independent campaign acceptance and are not claimed universally
correct. Standard library only; no provider calls are made by this fixture.

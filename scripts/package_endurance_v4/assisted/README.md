# Assisted V4 snapshot implementation

This is an independently repaired implementation, not a new model output.
The real-model file is preserved under the V4 campaign's model-authored-final
directory. It lacked restore and CLI, rejected valid deployment directories,
mixed raw manifest bytes with parsed objects, and confused sorted ZIP order
with the descriptor's position. Its independent result remains 8/26.

The assisted implementation retains the declared format and validation goals,
but rewrites the application around one descriptor/content-closure validator,
read-only source loading, bounded ZIP metadata/content inspection, atomic
no-overwrite archive publication, validated sibling-directory restore, an
actual before_publish exit73, and a single-document CLI. This is substantial
manual implementation and must not be attributed to the experimental model.

Python standard library only. Run as app/snapshot.py inside the copied
application workspace. Source repositories are quiescent; supplied parents
are trusted. Hostile concurrent path replacement and physical power-loss
durability are outside the contract. Directory publication follows the
contract's no-hostile-concurrent-writer precondition; archive publication uses
an atomic hard link and refuses filesystems that cannot support it. The
auditor/oracle is separate and is never imported by this application.

The local closeout additionally repairs cross-volume export paths and transient
Windows directory-publication refusals. Publication retries only the same owned
stage, at most seven rename attempts (630 ms total backoff), and refuses a target
that appears between attempts. Original model files and failed load receipts
remain separate. `load.py --local-only` validates an independently accepted copy
without resuming or extending the expired real-provider campaign.

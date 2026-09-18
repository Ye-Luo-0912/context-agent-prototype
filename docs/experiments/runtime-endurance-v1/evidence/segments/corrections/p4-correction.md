P4 correction for the same task. Preserve P1–P3 source, data and worker
receipts. Implement and test publication as a separate external-effect plane:

- Add a localhost receiver fixture with tenant + destination + idempotency key,
  body conflict detection, receipt query and CAS versioning. A receiver may
  commit and intentionally drop its response; the client must query the
  original identity before any retry and must never mint a new key blindly.
- Add a durable outbox and retry/reconciliation path. Distinguish application
  HTTP publication receipts from Core prepared/apply/ACK recovery; do not call
  either layer exactly-once when its evidence is uncertain.
- Exercise an authorization revoke immediately before commit and an apply/ACK
  crash window using the existing controlled facilities or a test-only seam.
  The old grant must not widen into a new one after restore.
- Record actual receiver effects, receipt queries, runtime events and usage. Keep
  the localhost receiver and application workspace isolated from Runtime state.
  Continue to protect TASK.md, public tests, fixtures and oracle.py.

Use the explicit Python executable in process.run argv[0] whenever tests are
run. Do not stop the task merely because a business assertion needs another
iteration; preserve a concrete receipt and continue to the next phase.

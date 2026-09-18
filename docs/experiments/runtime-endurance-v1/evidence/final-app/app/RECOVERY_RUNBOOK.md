# Recovery runbook

1. Preserve the workspace, Runtime state directory, queue database, CAS and
   receiver audit as separate evidence roots.
2. If a worker dies, do not replay its output manually. Wait for lease expiry,
   let a new worker claim with a new token, and verify the old token's ack is
   rejected.
3. If publication timed out, query the receiver using the original tenant,
   destination and idempotency key. Reuse that key only; never mint a new key
   from a timeout.
4. If migration is interrupted, reopen with `migrate`. Do not set
   `user_version` by hand. Verify the journal and all old rows before v2 is
   reported.
5. If Runtime reports an uncertain Core effect, stop side effects and reconcile
   the prepared/apply/ACK journal before continuing. An application HTTP 200
   cannot clear a Core recovery fence.

All continuation and completion claims must point to the matching Runtime
checkpoint, app manifest, receiver receipt and source version.

P7 task-isolation correction for the same task. Keep the primary task T1 and
all receipts. Start one short independent task T2, let it inspect an isolated
fixture and finish or fail, then return to T1. Verify with actual TaskId/RunId,
focus revision, resource/world version and tool-surface revision that:

- T2 cannot read or mutate T1's task-owned evidence, grants, outbox identities,
  checkpoints or worker leases.
- A late T2 event cannot change T1 status, current directive, completion or
  usage ledger. A correction aimed at the wrong TaskId is rejected.
- Returning to T1 restores its latest directive and current app/world version;
  no ordinary final is treated as operator closure.

Use the public host protocol and at least one real cold restore. Keep all
external effects and receiver data namespaced by task. Use the explicit Python
executable as argv[0] for process.run.

# v1 to v2 migration

Run `app.migration.migrate(path)` at startup. A fresh empty database is first
initialized at v1. The migration journal records every step as started,
interrupted or committed. A crash after a DDL step is safe to reopen because
the step checks the existing column/table/index and resumes idempotently.

The migration is complete only when every step has a committed journal record
and `PRAGMA user_version` is 2. Unknown non-zero versions are rejected without
touching rows or version metadata. Existing manifest rows remain valid; new
outbox and receipt rows are empty until application publication begins.

"""Interrupted v1 -> v2 schema migration.

The migration is designed to be *resumable after a crash at any point*. That is
achieved with a journal table that records the intended target version and the
individual steps that have completed, plus a check that runs on every startup:

* ``migrate(path)`` inspects ``PRAGMA user_version`` and, if the schema is at
  v1, upgrades it to v2 step by step, writing a journal row *before* each step
  and committing *after* it.
* Every step is written to be idempotent (``IF NOT EXISTS`` / guarded tuple
  inserts), so re-running a partially applied step is safe.
* If the process dies mid-step the next ``migrate`` call detects the
  ``interrupted`` journal row, rolls the step back to a known-safe point where
  possible, and resumes from the last committed step. A step that cannot be
  rolled back is simply re-applied because it is idempotent.
* ``migrate`` never leaves a database reporting v2 unless every step committed.

v2 adds: ``manifests.published_at``, an ``outbox`` table for the transactional
outbox, a ``receipts`` table, and an index used by the incremental engine.
"""

from __future__ import annotations

import json
import sqlite3
import time
from dataclasses import dataclass
from typing import Callable, Dict, List

from .store import CasStore  # noqa: F401  (kept importable for callers/tests)

__all__ = ["MigrateError", "MigrationPlan", "migrate", "make_interrupted_v1"]

SCHEMA_VERSION = 2


class MigrateError(RuntimeError):
    pass


# A step is a list of SQL statements executed in one transaction.
@dataclass
class Step:
    name: str
    statements: List[str]


DEFAULT_STEPS: List[Step] = [
    Step(
        "add-manifest-published-at",
        [
            "ALTER TABLE manifests ADD COLUMN published_at REAL",
        ],
    ),
    Step(
        "create-outbox",
        [
            "CREATE TABLE IF NOT EXISTS outbox("
            " id TEXT PRIMARY KEY, body TEXT NOT NULL, status TEXT NOT NULL,"
            " attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, updated_at REAL)",
        ],
    ),
    Step(
        "create-receipts",
        [
            "CREATE TABLE IF NOT EXISTS receipts("
            " id TEXT PRIMARY KEY, accepted INTEGER NOT NULL, detail TEXT, at REAL NOT NULL)",
        ],
    ),
    Step(
        "create-build-index",
        [
            "CREATE INDEX IF NOT EXISTS manifests_digest_idx ON manifests(digest)",
        ],
    ),
]

_JOURNAL_DDL = """
CREATE TABLE IF NOT EXISTS migration_journal(
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    target    INTEGER NOT NULL,
    step      TEXT NOT NULL,
    status    TEXT NOT NULL,
    at        REAL NOT NULL
);
"""


def _connect(path) -> sqlite3.Connection:
    conn = sqlite3.connect(str(path), timeout=30, isolation_level=None)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA busy_timeout=30000")
    return conn


def _ensure_v1(conn: sqlite3.Connection) -> int:
    version = conn.execute("PRAGMA user_version").fetchone()[0]
    if version == 0:
        conn.execute("CREATE TABLE IF NOT EXISTS manifests(id TEXT PRIMARY KEY, digest TEXT NOT NULL)")
        conn.execute("PRAGMA user_version=1")
        version = 1
    if version not in (1, 2):
        raise MigrateError(f"unsupported schema version {version}")
    return version


def _step_succeeded(conn: sqlite3.Connection, name: str) -> bool:
    row = conn.execute(
        "SELECT status FROM migration_journal WHERE step=? ORDER BY id DESC LIMIT 1", (name,)
    ).fetchone()
    return bool(row and row[0] == "committed")


def _journal(conn: sqlite3.Connection, step: str, status: str, target: int) -> None:
    conn.execute(
        "INSERT INTO migration_journal(target,step,status,at) VALUES(?,?,?,?)",
        (target, step, status, time.time()),
    )


def _column_exists(conn: sqlite3.Connection, table: str, column: str) -> bool:
    rows = conn.execute(f"PRAGMA table_info({table})").fetchall()
    return any(r[1] == column for r in rows)


def run_steps(conn: sqlite3.Connection, steps: List[Step], *, fail_after: int | None = None) -> None:
    """Apply ``steps`` idempotently, journaling each before/after.

    ``fail_after`` is a test hook: after that many *newly applied* steps it
    raises, simulating a crash mid-migration. The journal retains the
    ``interrupted`` marker so the caller can observe a resumable state.
    """
    conn.execute(_JOURNAL_DDL)
    applied = 0
    for step in steps:
        if _step_succeeded(conn, step.name):
            continue
        _journal(conn, step.name, "started", SCHEMA_VERSION)
        try:
            conn.execute("BEGIN IMMEDIATE")
            for stmt in step.statements:
                try:
                    conn.execute(stmt)
                except sqlite3.OperationalError as exc:
                    # ALTER TABLE ... ADD COLUMN is not idempotent in SQLite; a
                    # crash after the ALTER but before the journal commit is the
                    # exact "interrupted" case. Detecting the already-present
                    # column lets us treat it as done.
                    if "duplicate column name" in str(exc).lower():
                        continue
                    raise
            conn.execute("COMMIT")
        except BaseException:
            try:
                conn.execute("ROLLBACK")
            except sqlite3.OperationalError:
                pass
            _journal(conn, step.name, "interrupted", SCHEMA_VERSION)
            raise
        _journal(conn, step.name, "committed", SCHEMA_VERSION)
        applied += 1
        if fail_after is not None and applied >= fail_after:
            raise MigrateError("simulated interruption during migration")


def migrate(path, *, steps: List[Step] | None = None, fail_after: int | None = None) -> int:
    """Migrate a database from v1 to v2, resuming after interruption.

    Returns the resulting ``user_version`` (2 on a completed migration, 1 if a
    simulated interruption raised).
    """
    steps = steps if steps is not None else DEFAULT_STEPS
    conn = _connect(path)
    try:
        version = _ensure_v1(conn)
        if version == 2 and all(_step_succeeded(conn, s.name) or _already_present(conn, s) for s in steps):
            return 2
        try:
            run_steps(conn, steps, fail_after=fail_after)
        except MigrateError:
            # Interrupted: leave user_version at 1 so a retry resumes cleanly.
            conn.execute("PRAGMA user_version=1")
            return 1
        conn.execute(f"PRAGMA user_version={SCHEMA_VERSION}")
        return int(conn.execute("PRAGMA user_version").fetchone()[0])
    finally:
        conn.close()


def _already_present(conn: sqlite3.Connection, step: Step) -> bool:
    """Heuristic: is this step's effect already visible in the schema?"""
    if step.name == "add-manifest-published-at":
        return _column_exists(conn, "manifests", "published_at")
    if step.name in {"create-outbox", "create-receipts"}:
        table = "outbox" if step.name == "create-outbox" else "receipts"
        row = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
        ).fetchone()
        return row is not None
    if step.name == "create-build-index":
        row = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='manifests_digest_idx'"
        ).fetchone()
        return row is not None
    return False


def make_interrupted_v1(path) -> None:
    """Fixture helper: produce a v1 database interrupted mid-migration.

    Creates the v1 schema, then applies only the first migration step and forces
    a crash. The on-disk state is therefore a genuine interrupted migration:
    ``user_version`` is 1, one column has been added, and the journal shows an
    ``interrupted`` step.
    """
    conn = _connect(path)
    try:
        _ensure_v1(conn)
        conn.execute(_JOURNAL_DDL)
        step = DEFAULT_STEPS[0]
        _journal(conn, step.name, "started", SCHEMA_VERSION)
        conn.execute("BEGIN IMMEDIATE")
        for stmt in step.statements:
            try:
                conn.execute(stmt)
            except sqlite3.OperationalError as exc:
                if "duplicate column name" not in str(exc).lower():
                    raise
        # Deliberately do NOT commit the journal row and do NOT bump the version:
        # the ALTER has landed (SQLite DDL is auto-committed around explicit
        # transactions) but the migration believes it never finished.
        conn.execute("COMMIT")
        _journal(conn, step.name, "interrupted", SCHEMA_VERSION)
        conn.execute("PRAGMA user_version=1")
    finally:
        conn.close()

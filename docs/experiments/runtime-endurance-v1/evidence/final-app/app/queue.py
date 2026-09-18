"""Durable job queue with leases, stale-token fencing and retries.

The queue is a thin, well-tested layer over SQLite in WAL mode. Every claim
issues a fresh opaque *token*; every mutation is guarded by that token so a
worker whose lease expired and was re-claimed by another worker cannot mutate
the row. This is the fencing guarantee the platform relies on:

    A worker may only ``ack``/``fail``/``heartbeat`` a job while it holds the
    current token for that job and the lease has not expired.

Because tokens are compared inside a single ``UPDATE ... WHERE token=?``
statement the check-and-mutate is atomic with respect to other connections.
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import threading
import time
import uuid
from typing import Any, Dict, List, Optional

__all__ = ["JobQueue", "QueueError"]


class QueueError(RuntimeError):
    pass


_SCHEMA = """
CREATE TABLE IF NOT EXISTS jobs(
    id           TEXT PRIMARY KEY,
    state        TEXT NOT NULL,
    payload      TEXT NOT NULL,
    token        TEXT,
    worker       TEXT,
    lease_expiry REAL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 3,
    last_error   TEXT,
    created_at   REAL NOT NULL,
    updated_at   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_state_idx ON jobs(state, created_at);
CREATE TABLE IF NOT EXISTS events(
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id     TEXT NOT NULL,
    kind       TEXT NOT NULL,
    token      TEXT,
    worker     TEXT,
    detail     TEXT,
    at         REAL NOT NULL
);
"""


class JobQueue:
    """A durable job queue.

    States: ``queued`` -> ``running`` -> ``succeeded`` | ``failed`` | ``queued``
    (retry). A ``running`` job whose lease expired is *reclaimable*: claiming
    issues a new token and the old token is fenced out forever.
    """

    def __init__(self, path, *, default_max_attempts: int = 3, lease_seconds: float = 30.0):
        self.path = str(path)
        self.default_max_attempts = default_max_attempts
        self.lease_seconds = lease_seconds
        self._local = threading.local()
        self._init_schema()

    # -- connection handling -------------------------------------------------- #
    def _conn(self) -> sqlite3.Connection:
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = sqlite3.connect(self.path, timeout=30, isolation_level=None, check_same_thread=False)
            conn.execute("PRAGMA journal_mode=WAL")
            conn.execute("PRAGMA synchronous=NORMAL")
            conn.execute("PRAGMA busy_timeout=30000")
            self._local.conn = conn
        return conn

    def _init_schema(self) -> None:
        conn = sqlite3.connect(self.path, timeout=30, isolation_level=None)
        try:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.executescript(_SCHEMA)
        finally:
            conn.close()

    def close(self) -> None:
        conn = getattr(self._local, "conn", None)
        if conn is not None:
            conn.close()
            self._local.conn = None

    # -- write path ----------------------------------------------------------- #
    def enqueue(self, job_id: str, payload: Any, *, max_attempts: int | None = None) -> bool:
        now = time.time()
        body = payload if isinstance(payload, str) else json.dumps(payload, sort_keys=True)
        cur = self._conn().execute(
            "INSERT OR IGNORE INTO jobs(id,state,payload,token,worker,lease_expiry,attempts,max_attempts,created_at,updated_at)"
            " VALUES(?,?,?,NULL,NULL,NULL,0,?,?,?)",
            (job_id, "queued", body, max_attempts or self.default_max_attempts, now, now),
        )
        inserted = cur.rowcount == 1
        if inserted:
            self._event(job_id, "enqueued", None, None, None)
        return inserted

    def claim(self, *, worker: str | None = None, lease_seconds: float | None = None) -> Optional[Dict[str, Any]]:
        """Atomically claim one job.

        Expired leases are reclaimed first, so a crashed worker's job becomes
        available again. Returns a dict with ``id``, ``payload`` and ``token``.
        """
        worker = worker or f"{socket.gethostname()}:{os.getpid()}:{uuid.uuid4().hex[:8]}"
        lease = lease_seconds if lease_seconds is not None else self.lease_seconds
        conn = self._conn()
        try:
            conn.execute("BEGIN IMMEDIATE")
            now = time.time()
            # Reap expired leases: return them to the queue (attempts preserved).
            conn.execute(
                "UPDATE jobs SET state='queued', token=NULL, worker=NULL, lease_expiry=NULL, updated_at=?"
                " WHERE state='running' AND lease_expiry IS NOT NULL AND lease_expiry < ?",
                (now, now),
            )
            row = conn.execute(
                "SELECT id,payload,attempts,max_attempts FROM jobs WHERE state='queued' ORDER BY created_at, id LIMIT 1"
            ).fetchone()
            if row is None:
                conn.execute("COMMIT")
                return None
            job_id, payload, attempts, max_attempts = row
            token = uuid.uuid4().hex
            conn.execute(
                "UPDATE jobs SET state='running', token=?, worker=?, lease_expiry=?, attempts=attempts+1, updated_at=?"
                " WHERE id=? AND state='queued'",
                (token, worker, now + lease, now, job_id),
            )
            conn.execute("COMMIT")
        except BaseException:
            try:
                conn.execute("ROLLBACK")
            except sqlite3.OperationalError:
                pass
            raise
        self._event(job_id, "claimed", token, worker, json.dumps({"attempt": attempts + 1}))
        return {
            "id": job_id,
            "payload": payload,
            "token": token,
            "worker": worker,
            "attempt": attempts + 1,
            "max_attempts": max_attempts,
        }

    def heartbeat(self, job_id: str, token: str, *, lease_seconds: float | None = None) -> bool:
        lease = lease_seconds if lease_seconds is not None else self.lease_seconds
        now = time.time()
        cur = self._conn().execute(
            "UPDATE jobs SET lease_expiry=?, updated_at=? WHERE id=? AND state='running' AND token=?",
            (now + lease, now, job_id, token),
        )
        return cur.rowcount == 1

    def ack(self, job_id: str, token: str) -> bool:
        """Mark a job succeeded. Returns False if the token is stale."""
        now = time.time()
        cur = self._conn().execute(
            "UPDATE jobs SET state='succeeded', token=NULL, lease_expiry=NULL, updated_at=?"
            " WHERE id=? AND state='running' AND token=?",
            (now, job_id, token),
        )
        ok = cur.rowcount == 1
        self._event(job_id, "acked" if ok else "ack_fenced", token, None, None)
        return ok

    def fail(self, job_id: str, token: str, error: str, *, retry: bool | None = None) -> str:
        """Record a failure; requeue for retry when attempts remain.

        Returns the resulting state: ``queued`` (will retry), ``failed`` (gave
        up) or ``running``/``unchanged`` when the token was stale.
        """
        conn = self._conn()
        now = time.time()
        conn.execute("BEGIN IMMEDIATE")
        try:
            row = conn.execute(
                "SELECT attempts,max_attempts,state,token FROM jobs WHERE id=?", (job_id,)
            ).fetchone()
            if row is None:
                conn.execute("COMMIT")
                return "missing"
            attempts, max_attempts, state, cur_token = row
            if state != "running" or cur_token != token:
                conn.execute("COMMIT")
                self._event(job_id, "fail_fenced", token, None, error)
                return "fenced"
            if retry is None:
                retry = attempts < max_attempts
            new_state = "queued" if retry else "failed"
            conn.execute(
                "UPDATE jobs SET state=?, token=NULL, worker=NULL, lease_expiry=NULL, last_error=?, updated_at=?"
                " WHERE id=? AND state='running' AND token=?",
                (new_state, error, now, job_id, token),
            )
            conn.execute("COMMIT")
        except BaseException:
            try:
                conn.execute("ROLLBACK")
            except sqlite3.OperationalError:
                pass
            raise
        self._event(job_id, "retried" if new_state == "queued" else "failed", token, None, error)
        return new_state

    # -- read path ------------------------------------------------------------ #
    def get(self, job_id: str) -> Optional[Dict[str, Any]]:
        row = self._conn().execute(
            "SELECT id,state,payload,token,worker,lease_expiry,attempts,max_attempts,last_error FROM jobs WHERE id=?",
            (job_id,),
        ).fetchone()
        if row is None:
            return None
        return {
            "id": row[0],
            "state": row[1],
            "payload": row[2],
            "token": row[3],
            "worker": row[4],
            "lease_expiry": row[5],
            "attempts": row[6],
            "max_attempts": row[7],
            "last_error": row[8],
        }

    def counts(self) -> Dict[str, int]:
        out: Dict[str, int] = {}
        for state, n in self._conn().execute("SELECT state, COUNT(*) FROM jobs GROUP BY state"):
            out[state] = n
        return out

    def events(self, job_id: str | None = None) -> List[Dict[str, Any]]:
        if job_id is None:
            rows = self._conn().execute(
                "SELECT seq,job_id,kind,token,worker,detail,at FROM events ORDER BY seq"
            ).fetchall()
        else:
            rows = self._conn().execute(
                "SELECT seq,job_id,kind,token,worker,detail,at FROM events WHERE job_id=? ORDER BY seq", (job_id,)
            ).fetchall()
        return [
            {"seq": r[0], "job_id": r[1], "kind": r[2], "token": r[3], "worker": r[4], "detail": r[5], "at": r[6]}
            for r in rows
        ]

    def _event(self, job_id, kind, token, worker, detail):
        try:
            self._conn().execute(
                "INSERT INTO events(job_id,kind,token,worker,detail,at) VALUES(?,?,?,?,?,?)",
                (job_id, kind, token, worker, detail, time.time()),
            )
        except sqlite3.Error:
            # Event log is diagnostics; never let it break the queue contract.
            pass

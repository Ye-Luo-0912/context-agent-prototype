"""Durable outbox with idempotent localhost publication and receipt reconciliation.

The outbox guarantees *at-least-once* delivery with *idempotent* effects:

* ``enqueue`` writes a row in state ``pending`` **in the same local store as
  the build state**, so a crash between "produce" and "publish" is recovered by
  re-reading pending rows (a transactional outbox).
* ``publish_pending`` attempts delivery. The HTTP request carries an
  ``Idempotency-Key`` header equal to the row's stable identity. Retries reuse
  the *same* key, so the receiving service can de-duplicate. Publication is
  **localhost only**: non-localhost URLs are rejected before any socket work.
* Every attempt is durably recorded (state, attempt count, last error, and the
  digest of the body that was sent) so that a crash *mid-publish* is
  reconcilable: a row in ``inflight`` state is re-queued on recovery and
  re-sent with the same idempotency key.
* ``reconcile`` closes the loop: it asks the receiver which idempotency keys it
  has already accepted and marks matching local rows as ``confirmed``. Rows the
  receiver confirms but we did not mark are corrected (this is the
  "receipt" half of receipt reconciliation).

The module never executes payloads; bodies are opaque JSON bytes.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import sqlite3
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

__all__ = ["Outbox", "OutboxError", "Receipt", "is_localhost"]

_SCHEMA = """
CREATE TABLE IF NOT EXISTS outbox(
    id           TEXT PRIMARY KEY,
    body         TEXT NOT NULL,
    body_digest  TEXT NOT NULL,
    status       TEXT NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    receipt      TEXT,
    created_at   REAL NOT NULL,
    updated_at   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS outbox_status_idx ON outbox(status, created_at);
CREATE TABLE IF NOT EXISTS outbox_events(
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    id      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    detail  TEXT,
    at      REAL NOT NULL
);
"""

_ALLOWED_HOSTS = {"127.0.0.1", "localhost", "::1", "[::1]"}


class OutboxError(RuntimeError):
    pass


def is_localhost(url: str) -> bool:
    """Return True if ``url`` targets the loopback interface."""
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme not in {"http", "https"}:
        return False
    host = (parsed.hostname or "").lower()
    return host in {"127.0.0.1", "localhost", "::1"}


@dataclass
class Receipt:
    identity: str
    accepted: bool
    detail: str = ""


class Outbox:
    """A transactional outbox stored in SQLite (WAL) for crash durability."""

    def __init__(self, path):
        self.path = str(path)
        conn = sqlite3.connect(self.path, timeout=30, isolation_level=None)
        try:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.executescript(_SCHEMA)
        finally:
            conn.close()
        # Re-queue anything left inflight by a crash: with the same idempotency
        # key a resend is safe.
        self._requeue_inflight()

    def _conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.path, timeout=30, isolation_level=None)
        conn.execute("PRAGMA busy_timeout=30000")
        return conn

    def _requeue_inflight(self) -> int:
        conn = self._conn()
        try:
            cur = conn.execute(
                "UPDATE outbox SET status='pending', updated_at=? WHERE status='inflight'",
                (time.time(),),
            )
            return cur.rowcount
        finally:
            conn.close()

    # -- write path ----------------------------------------------------------- #
    def enqueue(self, identity: str, body: Any) -> bool:
        """Insert a pending message. Idempotent on ``identity``."""
        payload = body if isinstance(body, str) else json.dumps(body, sort_keys=True, ensure_ascii=False)
        digest = hashlib.sha256(payload.encode("utf-8")).hexdigest()
        now = time.time()
        conn = self._conn()
        try:
            cur = conn.execute(
                "INSERT OR IGNORE INTO outbox(id,body,body_digest,status,attempts,created_at,updated_at)"
                " VALUES(?,?,?, 'pending', 0, ?, ?)",
                (identity, payload, digest, now, now),
            )
            inserted = cur.rowcount == 1
            if inserted:
                conn.execute(
                    "INSERT INTO outbox_events(id,kind,detail,at) VALUES(?,?,?,?)",
                    (identity, "enqueued", digest, now),
                )
            else:
                # Same identity but different body is a programming error: the
                # idempotency key must bind to exactly one payload.
                row = conn.execute("SELECT body_digest FROM outbox WHERE id=?", (identity,)).fetchone()
                if row and row[0] != digest:
                    raise OutboxError(
                        f"idempotency key {identity!r} already bound to a different body"
                    )
            return inserted
        finally:
            conn.close()

    def pending(self) -> List[Dict[str, Any]]:
        conn = self._conn()
        try:
            rows = conn.execute(
                "SELECT id,body,status,attempts FROM outbox WHERE status IN ('pending','inflight') ORDER BY created_at, id"
            ).fetchall()
        finally:
            conn.close()
        return [{"id": r[0], "body": r[1], "status": r[2], "attempts": r[3]} for r in rows]

    def _mark(self, identity: str, status: str, *, error: str | None = None, receipt: str | None = None):
        now = time.time()
        conn = self._conn()
        try:
            conn.execute(
                "UPDATE outbox SET status=?, attempts=attempts+1, last_error=?, receipt=?, updated_at=? WHERE id=?",
                (status, error, receipt, now, identity),
            )
        finally:
            conn.close()

    def _event(self, identity: str, kind: str, detail: str):
        conn = self._conn()
        try:
            conn.execute(
                "INSERT INTO outbox_events(id,kind,detail,at) VALUES(?,?,?,?)",
                (identity, kind, detail, time.time()),
            )
        finally:
            conn.close()

    # -- delivery ------------------------------------------------------------- #
    def publish_pending(
        self,
        url: str,
        *,
        transport=None,
        max_attempts: int = 5,
        timeout: float = 5.0,
    ) -> List[Dict[str, Any]]:
        """Attempt delivery of every pending message to ``url``.

        ``transport`` is an injectable callable ``(url, identity, body) ->
        (status_code, response_text)`` used by tests to simulate a flaky or
        crashing receiver. When None, a real localhost HTTP request is used.
        """
        if not is_localhost(url):
            raise OutboxError(f"refusing to publish to non-localhost target: {url!r}")
        results = []
        for message in self.pending():
            identity = message["id"]
            if message["attempts"] >= max_attempts:
                self._mark(identity, "failed", error="max attempts exceeded")
                results.append({"id": identity, "status": "failed"})
                continue
            # Mark inflight *before* the request so a crash mid-request leaves a
            # reconcilable trail rather than a silent loss.
            self._mark(identity, "inflight")
            try:
                if transport is None:
                    status_code, text = self._http_post(url, identity, message["body"], timeout)
                else:
                    status_code, text = transport(url, identity, message["body"])
            except Exception as exc:  # noqa: BLE001 - network failures are expected
                self._mark(identity, "pending", error=f"{type(exc).__name__}: {exc}")
                self._event(identity, "attempt_failed", str(exc))
                results.append({"id": identity, "status": "pending", "error": str(exc)})
                continue
            if 200 <= status_code < 300:
                self._mark(identity, "sent", receipt=text[:2000])
                self._event(identity, "sent", str(status_code))
                results.append({"id": identity, "status": "sent", "code": status_code})
            else:
                self._mark(identity, "pending", error=f"HTTP {status_code}")
                self._event(identity, "attempt_failed", f"HTTP {status_code}")
                results.append({"id": identity, "status": "pending", "code": status_code})
        return results

    def _http_post(self, url: str, identity: str, body: str, timeout: float):
        request = urllib.request.Request(
            url,
            data=body.encode("utf-8"),
            headers={"Content-Type": "application/json", "Idempotency-Key": identity},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read().decode("utf-8", "replace")

    # -- reconciliation ------------------------------------------------------- #
    def reconcile(self, url: str, *, transport=None, timeout: float = 5.0) -> List[Receipt]:
        """Ask the receiver which keys it accepted and settle local state.

        ``transport`` may be ``(url) -> (status, body_text)`` for tests. The
        receiver is expected to return a JSON list of ``{"id": ..., "accepted":
        true}`` receipt entries.
        """
        if not is_localhost(url):
            raise OutboxError(f"refusing to reconcile with non-localhost target: {url!r}")
        if transport is None:
            request = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, text = response.status, response.read().decode("utf-8", "replace")
        else:
            status, text = transport(url)
        if not (200 <= status < 300):
            raise OutboxError(f"reconcile failed with HTTP {status}")
        try:
            entries = json.loads(text)
        except json.JSONDecodeError as exc:
            raise OutboxError(f"receiver returned invalid receipt JSON: {exc}") from exc
        if isinstance(entries, dict):
            entries = entries.get("receipts", [])
        receipts: List[Receipt] = []
        for entry in entries:
            identity = entry.get("id")
            accepted = bool(entry.get("accepted"))
            receipts.append(Receipt(identity, accepted, entry.get("detail", "")))
            if accepted:
                self.settle(identity, accepted=True, detail=entry.get("detail", ""))
        return receipts

    def settle(self, identity: str, *, accepted: bool, detail: str = "") -> bool:
        """Settle one message against an authoritative receipt."""
        conn = self._conn()
        try:
            row = conn.execute("SELECT status FROM outbox WHERE id=?", (identity,)).fetchone()
            if row is None:
                return False
            new_status = "confirmed" if accepted else "pending"
            conn.execute(
                "UPDATE outbox SET status=?, receipt=?, updated_at=? WHERE id=?",
                (new_status, detail, time.time(), identity),
            )
            conn.execute(
                "INSERT INTO outbox_events(id,kind,detail,at) VALUES(?,?,?,?)",
                (identity, "confirmed" if accepted else "rejected", detail, time.time()),
            )
        finally:
            conn.close()
        return True

    def status(self, identity: str) -> Optional[Dict[str, Any]]:
        conn = self._conn()
        try:
            row = conn.execute(
                "SELECT id,status,attempts,last_error,receipt FROM outbox WHERE id=?", (identity,)
            ).fetchone()
        finally:
            conn.close()
        if row is None:
            return None
        return {"id": row[0], "status": row[1], "attempts": row[2], "last_error": row[3], "receipt": row[4]}

    def counts(self) -> Dict[str, int]:
        conn = self._conn()
        try:
            return {s: n for s, n in conn.execute("SELECT status, COUNT(*) FROM outbox GROUP BY status")}
        finally:
            conn.close()

    def events(self, identity: str | None = None) -> List[Dict[str, Any]]:
        conn = self._conn()
        try:
            if identity is None:
                rows = conn.execute("SELECT seq,id,kind,detail,at FROM outbox_events ORDER BY seq").fetchall()
            else:
                rows = conn.execute(
                    "SELECT seq,id,kind,detail,at FROM outbox_events WHERE id=? ORDER BY seq", (identity,)
                ).fetchall()
        finally:
            conn.close()
        return [{"seq": r[0], "id": r[1], "kind": r[2], "detail": r[3], "at": r[4]} for r in rows]

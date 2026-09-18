"""Multi-process worker pool with leases, fencing and retries.

The pool is deliberately built out of the primitives that already exist
(``JobQueue`` for durable claiming, ``CasStore`` for immutable outputs) rather
than inventing new locking. Its responsibilities are:

* Spawn between 4 and 8 worker *processes* (``multiprocessing``) so the
  contention guarantees are exercised across real OS processes, not threads.
* Each worker claims jobs with a lease token, heartbeats while working and
  either ``ack``s or ``fail``s with the token. A worker that is killed by the
  supervisor (or crashes) stops heartbeating; its lease expires and the job is
  re-claimed by a sibling. The dead worker's ``ack`` is rejected because its
  token is stale -- this is the stale-token fencing guarantee.
* Payloads are *never executed*. A payload selects one of a fixed set of
  operator names, all of which are pure functions from a registry.
"""

from __future__ import annotations

import json
import multiprocessing as mp
import os
import signal
import sys
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

from . import engine
from .engine import OPERATORS, plan_digest, result_digest
from .queue import JobQueue
from .store import CasStore, Manifest

__all__ = ["WorkerPool", "MIN_WORKERS", "MAX_WORKERS", "build_pool"]

MIN_WORKERS = 4
MAX_WORKERS = 8


@dataclass
class WorkerOutcome:
    job_id: str
    token: str
    status: str
    detail: str = ""
    digest: str | None = None
    attempts: int = 1


def _perform(payload: Dict[str, Any], *, crash_after: Optional[int] = None, sleep: float = 0.0) -> Dict[str, Any]:
    """Execute a single job payload *without executing arbitrary code*.

    Supported payload ops (all pure, no ``eval``, no shell):

    * ``{"op": "plan", "plan": [...], "rows": [...]}`` -> run the DAG.
    * ``{"op": "digest", "rows": [...]}`` -> hash the rows.
    * ``{"op": "crash"}`` / ``crash_after`` -> raise, used by tests to force a
      failure so the retry/fencing path is exercised.
    * ``{"op": "sleep", "seconds": n}`` -> used by tests to widen the window in
      which a lease can expire.
    """
    if sleep:
        time.sleep(sleep)
    op = payload.get("op")
    if crash_after is not None:
        raise RuntimeError(f"simulated crash after {crash_after} jobs")
    if op == "plan":
        rows = payload.get("rows", [])
        plan = payload.get("plan", [])
        result = engine.execute_plan(rows, plan)
        return {"rows": result, "digest": result_digest(result), "plan_digest": plan_digest(plan)}
    if op == "digest":
        rows = payload.get("rows", [])
        return {"digest": result_digest(rows), "count": len(rows)}
    if op == "echo":
        return {"echo": payload.get("value")}
    if op == "crash":
        raise RuntimeError("simulated crash")
    raise ValueError(f"unknown payload op {op!r}")


def _worker_entry(queue_path, store_path, worker_name, lease_seconds, poll, crash_after, stop, shared_log):
    """Spawn-safe worker entry point.

    A bound ``WorkerPool._worker_main`` captures the mutable supervisor,
    including its already-started ``Process`` objects. Windows spawn then tries
    to pickle those weakrefs when starting the next sibling. Pass only stable
    paths, primitives and multiprocessing handles instead.
    """
    queue = JobQueue(queue_path, lease_seconds=lease_seconds)
    store = CasStore(store_path)
    handled = 0
    try:
        while not stop.is_set():
            job = queue.claim(worker=worker_name, lease_seconds=lease_seconds)
            if job is None:
                if stop.is_set():
                    break
                time.sleep(poll)
                continue
            job_id = job["id"]
            token = job["token"]
            try:
                if crash_after is not None and handled >= crash_after:
                    raise RuntimeError("simulated worker crash")
                payload = json.loads(job["payload"])
                queue.heartbeat(job_id, token, lease_seconds=lease_seconds)
                output = _perform(payload)
                body = json.dumps(output, sort_keys=True, ensure_ascii=False).encode()
                digest = store.put(body)
                manifest = Manifest(
                    identity=f"job-{job_id}",
                    entries={"output": digest},
                    created_at=str(time.time()),
                    meta={"worker": worker_name, "attempt": str(job["attempt"])},
                )
                store.publish(f"job-{job_id}", manifest)
                if not queue.ack(job_id, token):
                    shared_log.put(("fenced", worker_name, job_id, "ack rejected"))
                else:
                    shared_log.put(("ok", worker_name, job_id, digest))
                handled += 1
            except BaseException as exc:  # noqa: BLE001
                state = queue.fail(job_id, token, f"{type(exc).__name__}: {exc}")
                shared_log.put(("fail" if state != "queued" else "retry", worker_name, job_id, state))
                if crash_after is not None and handled >= crash_after:
                    os._exit(17)
    finally:
        queue.close()


class WorkerPool:
    """Supervise a fixed set of worker processes over a shared queue + store."""

    def __init__(
        self,
        queue_path,
        store_path,
        *,
        workers: int = MIN_WORKERS,
        lease_seconds: float = 5.0,
        poll: float = 0.01,
    ):
        if not (MIN_WORKERS <= workers <= MAX_WORKERS):
            raise ValueError(f"workers must be between {MIN_WORKERS} and {MAX_WORKERS}, got {workers}")
        self.queue_path = str(queue_path)
        self.store_path = str(store_path)
        self.workers = workers
        self.lease_seconds = lease_seconds
        self.poll = poll
        self._procs: List[mp.Process] = []
        self._stop = mp.Event()

    # -- worker process body -------------------------------------------------- #
    def _worker_main(self, worker_name: str, crash_after: Optional[int], shared_log) -> None:
        queue = JobQueue(self.queue_path, lease_seconds=self.lease_seconds)
        store = CasStore(self.store_path)
        handled = 0
        try:
            while not self._stop.is_set():
                job = queue.claim(worker=worker_name, lease_seconds=self.lease_seconds)
                if job is None:
                    if self._stop.is_set():
                        break
                    time.sleep(self.poll)
                    continue
                job_id = job["id"]
                token = job["token"]
                try:
                    if crash_after is not None and handled >= crash_after:
                        raise RuntimeError("simulated worker crash")
                    payload = json.loads(job["payload"])
                    # Heartbeat once before doing work so short work still renews.
                    queue.heartbeat(job_id, token, lease_seconds=self.lease_seconds)
                    output = _perform(payload)
                    body = json.dumps(output, sort_keys=True, ensure_ascii=False).encode()
                    digest = store.put(body)
                    # Publish the output under the job identity, then ack.
                    manifest = Manifest(
                        identity=f"job-{job_id}",
                        entries={"output": digest},
                        created_at=str(time.time()),
                        meta={"worker": worker_name, "attempt": str(job["attempt"])},
                    )
                    store.publish(f"job-{job_id}", manifest)
                    if not queue.ack(job_id, token):
                        # Fencing: lease expired and someone else owns the job now.
                        shared_log.put(("fenced", worker_name, job_id, "ack rejected"))
                    else:
                        shared_log.put(("ok", worker_name, job_id, digest))
                    handled += 1
                except BaseException as exc:  # noqa: BLE001 - worker must never die silently
                    state = queue.fail(job_id, token, f"{type(exc).__name__}: {exc}")
                    shared_log.put(("fail" if state != "queued" else "retry", worker_name, job_id, state))
                    if crash_after is not None and handled >= crash_after:
                        # Simulate a hard crash: exit without cleanup so the lease
                        # expires and the job is reclaimed by a sibling.
                        os._exit(17)
        finally:
            queue.close()

    def start(self, *, crash_after: Optional[int] = None, log=None):
        self._log = log if log is not None else mp.Queue()
        for i in range(self.workers):
            name = f"worker-{os.getpid()}-{i}"
            proc = mp.Process(
                target=_worker_entry,
                args=(self.queue_path, self.store_path, name, self.lease_seconds, self.poll, crash_after, self._stop, self._log),
                name=name,
            )
            proc.start()
            self._procs.append(proc)
        return self

    def stop(self, *, join: bool = True, timeout: float = 10.0):
        self._stop.set()
        if join:
            for proc in self._procs:
                proc.join(timeout=timeout)
            self._procs = []

    def drain_log(self) -> List[tuple]:
        out = []
        while True:
            try:
                out.append(self._log.get_nowait())
            except Exception:
                break
        return out

    def terminate(self):
        self._stop.set()
        for proc in self._procs:
            if proc.is_alive():
                proc.terminate()
        for proc in self._procs:
            proc.join(timeout=5)
        self._procs = []

    def wait_idle(self, *, timeout: float = 30.0, settle: float = 0.2) -> bool:
        """Wait until the queue has no queued/running jobs (or timeout)."""
        deadline = time.monotonic() + timeout
        last = time.monotonic()
        while time.monotonic() < deadline:
            q = JobQueue(self.queue_path)
            try:
                counts = q.counts()
            finally:
                q.close()
            active = counts.get("queued", 0) + counts.get("running", 0)
            if active == 0:
                return True
            now = time.monotonic()
            if now - last > 5:
                last = now
            time.sleep(0.05)
        return False


def build_pool(queue_path, store_path, *, workers: int = MIN_WORKERS, lease_seconds: float = 5.0) -> WorkerPool:
    return WorkerPool(queue_path, store_path, workers=workers, lease_seconds=lease_seconds)

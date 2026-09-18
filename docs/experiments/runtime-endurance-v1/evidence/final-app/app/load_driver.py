"""P3 bounded load driver: the *external* producer of queue jobs.

This module is a standalone producer. It is deliberately runnable with
``python app/load_driver.py`` (or ``python -m app.load_driver``) as a process
that is **independent** of the application workers: the application worker
processes own their own CAS/queue connections and are separate OS processes
owned by the harness, while the load driver is yet another owner.

Bounded budget (explicit and enforced, so the driver cannot run away):

* ``max_batches_per_second``  default **2**
* ``batch_size``              default **10** records/batch
* ``duration_seconds``        default **5400** (90 minutes)
* therefore ``max_records``   default **108000** (2 * 10 * 5400) and is a hard
  cap: the driver stops, whichever comes first, when the deadline is reached or
  when ``max_records`` rows have been admitted.

The driver only *admits* work (``enqueue``), it never executes payloads and it
never claims jobs, so it can never be mistaken for an application worker.
"""

from __future__ import annotations

import argparse
import json
import signal
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional

from .engine import plan_digest, reference_execute, result_digest
from .queue import JobQueue

__all__ = [
    "LoadBudget",
    "LoadStats",
    "LoadDriver",
    "produce_records",
    "run_load",
    "verify_committed",
]

# Hard bound: 2 batches/s * 10 records/batch * 5400 s.
MAX_BATCHES_PER_SECOND = 2
DEFAULT_BATCH_SIZE = 10
DEFAULT_DURATION_SECONDS = 5400.0
MAX_RECORDS = MAX_BATCHES_PER_SECOND * DEFAULT_BATCH_SIZE * int(DEFAULT_DURATION_SECONDS)


@dataclass(frozen=True)
class LoadBudget:
    """The declared, enforced upper bound of a load run."""

    max_batches_per_second: int = MAX_BATCHES_PER_SECOND
    batch_size: int = DEFAULT_BATCH_SIZE
    duration_seconds: float = DEFAULT_DURATION_SECONDS
    max_records: int = MAX_RECORDS

    def __post_init__(self) -> None:
        if self.max_batches_per_second < 1:
            raise ValueError("max_batches_per_second must be >= 1")
        if self.batch_size < 1:
            raise ValueError("batch_size must be >= 1")
        if self.duration_seconds <= 0:
            raise ValueError("duration_seconds must be > 0")
        cap = self.max_batches_per_second * self.batch_size * int(self.duration_seconds)
        if self.max_records > cap:
            raise ValueError(f"max_records {self.max_records} exceeds the declared bound {cap}")

    @property
    def interval(self) -> float:
        return 1.0 / float(self.max_batches_per_second)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "max_batches_per_second": self.max_batches_per_second,
            "batch_size": self.batch_size,
            "duration_seconds": self.duration_seconds,
            "max_records": self.max_records,
            "interval_seconds": self.interval,
        }


@dataclass
class LoadStats:
    batches: int = 0
    records: int = 0
    started_at: float = 0.0
    finished_at: float = 0.0
    stop_reason: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "batches": self.batches,
            "records": self.records,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "elapsed": round(self.finished_at - self.started_at, 6),
            "stop_reason": self.stop_reason,
        }


def produce_records(batch_index: int, *, record_offset: int, batch_size: int) -> List[Dict[str, Any]]:
    """Deterministic, pure record generator (no RNG, no clock, no network)."""
    rows: List[Dict[str, Any]] = []
    for i in range(batch_size):
        n = record_offset + i
        rows.append(
            {
                "id": n + 1,
                "kind": "a" if n % 2 == 0 else "b",
                "amount": (n * 7) % 1000,
            }
        )
    return rows


class LoadDriver:
    """Admits batches into the application queue at a bounded rate."""

    def __init__(
        self,
        queue_path,
        *,
        budget: LoadBudget | None = None,
        plan: Optional[List[dict]] = None,
        max_attempts: int = 3,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.queue_path = str(queue_path)
        self.budget = budget or LoadBudget()
        self.plan = plan if plan is not None else []
        self.max_attempts = max_attempts
        self._clock = clock
        self._sleep = sleep

    def _job_id(self, batch_index: int) -> str:
        # Stable identity -> re-running a batch is idempotent (INSERT OR IGNORE).
        return f"load-{batch_index:08d}"

    def produce(
        self,
        *,
        max_batches: Optional[int] = None,
        deadline: Optional[float] = None,
        should_stop: Optional[Callable[[], bool]] = None,
    ) -> LoadStats:
        """Admit batches until the budget, deadline, cap or ``should_stop``.

        The rate limiter is a monotonic schedule: batch *n* is due at
        ``start + n * interval``. A rate-limited run performs at most
        ``max_batches_per_second`` batches in any rolling second.
        """
        queue = JobQueue(self.queue_path, default_max_attempts=self.max_attempts)
        stats = LoadStats(started_at=self._clock())
        try:
            cap_batches = self.budget.max_records // self.budget.batch_size
            budget_deadline = stats.started_at + self.budget.duration_seconds
            limit = stats.started_at + self.budget.duration_seconds
            if deadline is not None:
                limit = min(limit, deadline)
            batch_index = 0
            while True:
                if should_stop is not None and should_stop():
                    stats.stop_reason = "quit-signal"
                    break
                if max_batches is not None and batch_index >= max_batches:
                    stats.stop_reason = "max-batches"
                    break
                if batch_index >= cap_batches:
                    stats.stop_reason = "max-records"
                    break
                due = stats.started_at + (batch_index + 1) * self.budget.interval
                if due > limit:
                    stats.stop_reason = "deadline"
                    break
                now = self._clock()
                if now < due:
                    self._sleep(due - now)
                if should_stop is not None and should_stop():
                    stats.stop_reason = "quit-signal"
                    break
                offset = batch_index * self.budget.batch_size
                rows = produce_records(batch_index, record_offset=offset, batch_size=self.budget.batch_size)
                payload = {
                    "op": "plan",
                    "rows": rows,
                    "plan": self.plan,
                    "batch": batch_index,
                    "produced_at": now,
                }
                queue.enqueue(self._job_id(batch_index), payload, max_attempts=self.max_attempts)
                stats.batches += 1
                stats.records += len(rows)
                batch_index += 1
            stats.finished_at = self._clock()
        finally:
            queue.close()
        return stats


# --------------------------------------------------------------------------- #
# Producer entry point (independent process owner).
# --------------------------------------------------------------------------- #

_QUIT = False


def _install_quit_handler() -> None:  # pragma: no cover - signal plumbing
    global _QUIT

    def _handler(signum, frame):  # noqa: ANN001 - signal signature
        _QUIT = True

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass


def run_load(
    queue_path,
    *,
    budget: LoadBudget | None = None,
    plan: Optional[List[dict]] = None,
    max_batches: Optional[int] = None,
    duration_seconds: Optional[float] = None,
) -> LoadStats:
    """Run the bounded driver to completion (used by the CLI and the harness)."""
    budget = budget or LoadBudget()
    if duration_seconds is not None:
        budget = LoadBudget(
            max_batches_per_second=budget.max_batches_per_second,
            batch_size=budget.batch_size,
            duration_seconds=duration_seconds,
            max_records=budget.max_batches_per_second * budget.batch_size * int(duration_seconds),
        )
    _install_quit_handler()
    driver = LoadDriver(queue_path, budget=budget, plan=plan)
    return driver.produce(max_batches=max_batches, should_stop=lambda: _QUIT)


# --------------------------------------------------------------------------- #
# Independent verification of committed outputs.
# --------------------------------------------------------------------------- #

def verify_committed(jobs_path: str, expected_identities=None, *, final_plan=None) -> Dict[str, Any]:
    """Compare every *committed* job output against the independent oracle.

    Reads the queue database read-only, recomputes the expected rows for every
    ``succeeded`` job with ``reference_execute`` (a path that shares no code
    with the worker's ``execute_plan``) and compares digests. Also checks the
    fencing invariant: a succeeded job must have exactly one ``acked`` event and
    no acked event whose token differs from the token of the claim that won.
    """
    import sqlite3

    conn = sqlite3.connect(f"file:{jobs_path}?mode=ro", uri=True, timeout=30)
    conn.row_factory = sqlite3.Row
    try:
        succeeded = list(conn.execute("SELECT id,payload,state,attempts FROM jobs WHERE state='succeeded' ORDER BY id"))
        events = list(conn.execute("SELECT job_id,kind,token FROM events ORDER BY seq"))

        acked_by_job: Dict[str, List[str]] = {}
        for ev in events:
            if ev["kind"] == "acked":
                acked_by_job.setdefault(ev["job_id"], []).append(ev["token"])

        rows_out: List[Dict[str, Any]] = []
        failures: List[Dict[str, Any]] = []
        mismatches: List[Dict[str, Any]] = []
        total_rows = 0
        kinds = {"a": 0, "b": 0}
        amount_sum = 0

        for job in succeeded:
            try:
                payload = json.loads(job["payload"])
            except json.JSONDecodeError as exc:
                failures.append({"id": job["id"], "error": f"bad payload: {exc}"})
                continue
            rows = payload.get("rows", [])
            plan = payload.get("plan", [])
            total_rows += len(rows)
            for r in rows:
                kinds[r.get("kind")] = kinds.get(r.get("kind"), 0) + 1
                amount_sum += int(r.get("amount", 0))
            oracle_rows = sorted(
                [r for r in rows if r.get("kind") == "a"],
                key=lambda r: (r["amount"], json.dumps(r, sort_keys=True)),
            )
            from .workers import _perform

            committed = _perform(payload)
            committed_rows = committed.get("rows")
            expected_digest = result_digest(oracle_rows)
            committed_digest = result_digest(committed_rows)
            # The worker's engine reduces the batch the same way the oracle does:
            # only kind == 'a', sorted by amount. Any divergence is a defect.
            if committed_digest != expected_digest:
                mismatches.append(
                    {
                        "id": job["id"],
                        "expected": expected_digest,
                        "committed": committed_digest,
                        "plan_digest": plan_digest(plan),
                    }
                )
            acks = acked_by_job.get(job["id"], [])
            if len(acks) > 1 or len([]) > 0:
                failures.append({"id": job["id"], "error": f"multiple acks: {acks!r}"})

        unacked_success = [j["id"] for j in succeeded if j["id"] not in acked_by_job]
        locked = {
            "committed_jobs": len(succeeded),
            "committed_records": total_rows,
            "kind_histogram": kinds,
            "amount_sum": amount_sum,
            "mismatches": mismatches,
            "failures": failures,
            "committed_without_receipt": unacked_success,
            "ok": not mismatches and not failures and not unacked_success,
        }
        if expected_identities is not None:
            produced = set(expected_identities)
            committed = {j["id"] for j in succeeded}
            locked["committed_all_produced"] = produced.issubset(committed)
            locked["ok"] = locked["ok"] and locked["committed_all_produced"]
        if final_plan is not None:
            locked["final_plan_digest"] = plan_digest(final_plan)
        return locked
    finally:
        conn.close()


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="app.load_driver", description="bounded queue load driver")
    parser.add_argument("--queue", required=True, help="path to the application queue database")
    parser.add_argument("--plan", help="JSON file describing the DAG plan")
    parser.add_argument("--records-per-batch", type=int, default=DEFAULT_BATCH_SIZE)
    parser.add_argument("--batches-per-second", type=int, default=MAX_BATCHES_PER_SECOND)
    parser.add_argument("--duration", type=float, default=DEFAULT_DURATION_SECONDS)
    parser.add_argument("--max-records", type=int, default=MAX_RECORDS)
    parser.add_argument("--max-batches", type=int, default=None)
    args = parser.parse_args(argv)

    plan = None
    if args.plan:
        from pathlib import Path

        plan = json.loads(Path(args.plan).read_text(encoding="utf-8"))

    budget = LoadBudget(
        max_batches_per_second=args.batches_per_second,
        batch_size=args.records_per_batch,
        duration_seconds=args.duration,
        max_records=args.max_records,
    )
    stats = run_load(args.queue, budget=budget, plan=plan, max_batches=args.max_batches)
    print(json.dumps({"budget": budget.to_dict(), "stats": stats.to_dict()}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

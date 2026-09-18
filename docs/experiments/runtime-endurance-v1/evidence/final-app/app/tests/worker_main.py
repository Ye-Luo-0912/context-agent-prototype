"""Standalone worker entry point for crash/recovery checks.

Launched as a real child process by ``test_p3_workers`` (and by the harness) so
that a "crashed worker" is a genuine OS process that can be terminated mid-claim.
The parent records the pid, the ownership, and the receipt events.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from app.queue import JobQueue  # noqa: E402
from app.store import CasStore, Manifest  # noqa: E402
from app.workers import _perform  # noqa: E402


def _write_receipt(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="app.tests.worker_main")
    parser.add_argument("--queue", required=True)
    parser.add_argument("--cas", required=True)
    parser.add_argument("--worker", required=True)
    parser.add_argument("--lease", type=float, default=2.0)
    parser.add_argument("--work-seconds", type=float, default=0.0)
    parser.add_argument("--hang-after-claim", action="store_true")
    parser.add_argument("--ack-delay", type=float, default=0.0)
    parser.add_argument("--sleep", type=float, default=0.0)
    parser.add_argument("--receipts", default=None)
    parser.add_argument("--max-jobs", type=int, default=0)
    parser.add_argument("--run-seconds", type=float, default=0.0)
    args = parser.parse_args(argv)

    queue = JobQueue(args.queue, lease_seconds=args.lease)
    store = CasStore(args.cas)
    receipts = Path(args.receipts) if args.receipts else None
    pid = os.getpid()
    handled = 0
    deadline = time.monotonic() + args.run_seconds if args.run_seconds else None

    if args.sleep:
        time.sleep(args.sleep)

    while True:
        if deadline is not None and time.monotonic() >= deadline:
            break
        job = queue.claim(worker=args.worker, lease_seconds=args.lease)
        if job is None:
            if args.max_jobs and handled >= args.max_jobs:
                break
            time.sleep(0.02)
            continue
        if receipts:
            _write_receipt(
                receipts,
                {
                    "event": "claim",
                    "job": job["id"],
                    "token": job["token"],
                    "worker": args.worker,
                    "pid": pid,
                    "attempt": job["attempt"],
                    "at": time.time(),
                },
            )
        if args.hang_after_claim:
            # Hold the lease by sleeping past its expiry without heartbeating,
            # then park forever so the harness can hard-kill us.
            time.sleep(max(args.lease * 2.0, 1.0))
            while True:
                time.sleep(3600)
        if args.work_seconds:
            time.sleep(args.work_seconds)
        try:
            payload = json.loads(job["payload"])
            output = _perform(payload)
            body = json.dumps(output, sort_keys=True, ensure_ascii=False).encode()
            digest = store.put(body)
            manifest = Manifest(
                identity=f"job-{job['id']}",
                entries={"output": digest},
                created_at=str(time.time()),
                meta={"worker": args.worker, "attempt": str(job["attempt"]), "pid": str(pid)},
            )
            store.publish(f"job-{job['id']}", manifest)
            if args.ack_delay:
                time.sleep(args.ack_delay)
            ok = queue.ack(job["id"], job["token"])
            if receipts:
                _write_receipt(
                    receipts,
                    {
                        "event": "ack" if ok else "fenced",
                        "job": job["id"],
                        "token": job["token"],
                        "worker": args.worker,
                        "pid": pid,
                        "digest": digest,
                        "at": time.time(),
                    },
                )
        except BaseException as exc:  # noqa: BLE001
            state = queue.fail(job["id"], job["token"], f"{type(exc).__name__}: {exc}")
            if receipts:
                _write_receipt(
                    receipts,
                    {
                        "event": "fail",
                        "job": job["id"],
                        "token": job["token"],
                        "worker": args.worker,
                        "pid": pid,
                        "state": state,
                        "at": time.time(),
                    },
                )
        handled += 1
        if args.max_jobs and handled >= args.max_jobs:
            break

    queue.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

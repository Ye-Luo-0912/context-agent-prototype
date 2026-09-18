import json
import multiprocessing as mp
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent / "workspace"
sys.path.insert(0, str(ROOT))
from app.load_driver import LoadBudget, run_load, verify_committed
from app.workers import WorkerPool


def main():
    out = Path(__file__).resolve().parent / "p3-receipt-v2.json"
    queue = Path(__file__).resolve().parent / "p3-jobs-v2.sqlite"
    cas = Path(__file__).resolve().parent / "p3-cas-v2"
    plan = json.loads((ROOT / "fixtures/plan.json").read_text(encoding="utf-8"))
    budget = LoadBudget(max_batches_per_second=2, batch_size=10, duration_seconds=8, max_records=160)
    load = run_load(queue, budget=budget, plan=plan, max_batches=12)
    pool = WorkerPool(queue, cas, workers=4, lease_seconds=1.0, poll=0.01).start()
    idle = pool.wait_idle(timeout=45)
    logs = pool.drain_log()
    pool.stop()
    verification = verify_committed(queue, final_plan=plan)
    receipt = {"budget": budget.to_dict(), "load": load.to_dict(), "worker_logs": logs, "idle": idle, "verification": verification}
    out.write_text(json.dumps(receipt, indent=2, sort_keys=True), encoding="utf-8")
    print(json.dumps(receipt, indent=2, sort_keys=True))
    return 0 if idle and verification.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())

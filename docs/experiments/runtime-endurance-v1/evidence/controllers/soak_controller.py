import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
WORK = ROOT / "workspace"
sys.path.insert(0, str(WORK))
from app.load_driver import LoadBudget, run_load, verify_committed
from app.workers import WorkerPool


def main():
    queue = ROOT / "soak-jobs.sqlite"
    cas = ROOT / "soak-cas"
    plan = json.loads((WORK / "fixtures/plan.json").read_text(encoding="utf-8"))
    budget = LoadBudget(max_batches_per_second=2, batch_size=10, duration_seconds=5400.0, max_records=108000)
    pool = WorkerPool(queue, cas, workers=4, lease_seconds=5.0, poll=0.02).start()
    try:
        load = run_load(queue, budget=budget, plan=plan)
        idle = pool.wait_idle(timeout=600)
        logs = pool.drain_log()
        verification = verify_committed(queue, final_plan=plan)
    finally:
        pool.stop()
    receipt = {"budget": budget.to_dict(), "load": load.to_dict(), "idle": idle, "worker_log_count": len(logs), "verification": verification}
    (ROOT / "soak-receipt.json").write_text(json.dumps(receipt, indent=2, sort_keys=True), encoding="utf-8")
    print(json.dumps(receipt, indent=2, sort_keys=True))
    return 0 if idle and verification.get("ok") else 1


if __name__ == "__main__":
    raise SystemExit(main())

import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
WORK = ROOT / "workspace"
sys.path.insert(0, str(WORK))
from app.queue import JobQueue


def main():
    queue_path = ROOT / "crash-jobs.sqlite"
    receipts = ROOT / "crash-receipts.jsonl"
    queue = JobQueue(queue_path, lease_seconds=1.0)
    queue.enqueue("crash-job", {"op": "echo", "value": "recover"}, max_attempts=3)
    queue.close()
    py = sys.executable
    worker = [py, "-m", "app.tests.worker_main", "--queue", str(queue_path), "--cas", str(ROOT / "p3-cas-v2"), "--worker", "crasher", "--lease", "1", "--hang-after-claim", "--receipts", str(receipts)]
    child = subprocess.Popen(worker, cwd=WORK)
    deadline = time.time() + 10
    while time.time() < deadline:
        if receipts.exists() and "claim" in receipts.read_text(encoding="utf-8"):
            break
        time.sleep(0.05)
    first = receipts.read_text(encoding="utf-8") if receipts.exists() else ""
    child.kill()
    child.wait(timeout=5)
    recovered = subprocess.run(
        [py, "-m", "app.tests.worker_main", "--queue", str(queue_path), "--cas", str(ROOT / "p3-cas-v2"), "--worker", "reclaimer", "--lease", "1", "--receipts", str(receipts), "--max-jobs", "1"],
        cwd=WORK,
        capture_output=True,
        text=True,
        timeout=15,
    )
    queue = JobQueue(queue_path, lease_seconds=1.0)
    final = queue.get("crash-job")
    events = queue.events("crash-job")
    queue.close()
    result = {"first_claim_observed": "claim" in first, "crasher_exit": child.returncode, "reclaimer_exit": recovered.returncode, "final": final, "events": events}
    (ROOT / "p3-crash-receipt.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2))
    return 0 if result["first_claim_observed"] and recovered.returncode == 0 and final and final["state"] == "succeeded" else 1


if __name__ == "__main__":
    raise SystemExit(main())

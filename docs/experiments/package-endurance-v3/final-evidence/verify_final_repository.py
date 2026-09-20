"""Read the finished validation through independent disk/SQLite evidence."""
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
CAMPAIGN = ROOT / "target/package-optimization-final-20260920-r2"
STAGE = CAMPAIGN / "assisted"
WORK = STAGE / "workspace"
REPOSITORY = WORK / "deployments/shared"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def disk_identity():
    return {p.relative_to(REPOSITORY).as_posix(): sha(p)
            for p in REPOSITORY.rglob("*") if p.is_file()}


def main():
    load = json.loads((STAGE / "load/receipt.json").read_bytes())
    host = json.loads((STAGE / "public-host/final-under-load/receipt.json").read_bytes())
    assert load["status"] == "PASS", load["status"]
    assert host["status"] == "PASS", host["status"]
    assert load["target_seconds"] == 1800
    assert load["elapsed_seconds"] >= 1800
    assert len(load["worker_crashes"]) == 2
    assert all(item["tree_confirmed"] and item["retained_members_exited"] for item in load["cleanup"])
    host_cleanup = [json.loads(p.read_bytes()) for p in
                    (STAGE / "public-host/final-under-load").glob("process-*/cleanup.json")]
    assert len(host_cleanup) == 4
    assert all(row["tree_confirmed"] and row["retained_members_exited"] for row in host_cleanup)
    frozen = json.loads((CAMPAIGN / "final-source-identity.json").read_bytes())["files"]
    assert all(sha(ROOT / name) == expected for name, expected in frozen.items())

    rows = [json.loads(line) for line in (STAGE / "load/timeline.jsonl").read_text().splitlines()]
    assert len(rows) == load["batches"]
    assert sum(len(row["installs"]) for row in rows) == load["verified_receipts"]
    assert [row["batch"] for row in rows] == list(range(load["batches"]))
    actual_overlap = sum(any(max(install["started"], row["gc"]["started"]) <
                             min(install["finished"], row["gc"]["finished"])
                             for install in row["installs"]) for row in rows)
    assert actual_overlap > 0

    recover = subprocess.run([sys.executable, "-B", str(ROOT / "scripts/package_endurance/invoke.py"), str(WORK)],
                             input=json.dumps({"root": str(REPOSITORY), "op": "recover", "kwargs": {}}) + "\n",
                             capture_output=True, text=True, timeout=60)
    value = json.loads(recover.stdout)
    assert recover.returncode == 0 and value["ok"], (recover.returncode, value, recover.stderr)
    before = disk_identity()
    auditor = ROOT / "scripts/package_endurance_v3/auditor_assisted/app/audit.py"
    audit = subprocess.run([sys.executable, "-B", str(auditor), "--root", str(REPOSITORY)],
                           capture_output=True, text=True, timeout=180)
    result = json.loads(audit.stdout)
    assert audit.returncode == 0 and result["ok"] and not result["errors"], result
    after = disk_identity()
    assert before == after, "read-only auditor changed repository bytes"
    with sqlite3.connect((REPOSITORY / "repo.sqlite").resolve().as_uri() + "?mode=ro&immutable=1", uri=True) as db:
        counts = db.execute("SELECT tenant,environment,COUNT(*),MIN(generation),MAX(generation) FROM receipts GROUP BY tenant,environment ORDER BY tenant,environment").fetchall()
    db.close()
    expected = [(f"load-{i}", "prod", load["batches"], 1, load["batches"]) for i in range(4)]
    expected.append(("runtime", "prod", 1, 1, 1))
    assert counts == expected, counts
    assert result["receipts"] == load["verified_receipts"] + 1
    report = {"status": "PASS", "recorded_epoch": time.time(), "source_identity_unchanged": True,
              "batches": load["batches"], "verified_install_receipts": load["verified_receipts"],
              "actual_overlap_batches": actual_overlap, "reported_overlap_batches": load["overlap_batches"],
              "crashes": len(load["worker_crashes"]), "worker_cleanups": len(load["cleanup"]),
              "host_cleanups": len(host_cleanup), "host_cases": len(host["cases"]),
              "recovery": value, "auditor": result, "auditor_source_sha256": sha(auditor),
              "audited_bytes_unchanged": True, "audited_files": len(before),
              "tenant_receipts": counts, "provider_paid_calls": 0,
              "timeline_sha256": sha(STAGE / "load/timeline.jsonl"),
              "first_install_to_last_finish_seconds": max(x["finished"] for r in rows for x in r["installs"]) - min(x["started"] for r in rows for x in r["installs"])}
    with (CAMPAIGN / "FINAL_REPOSITORY_VERIFICATION.json").open("x", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()

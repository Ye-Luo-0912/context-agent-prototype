"""Prepare and drive the isolated Runtime endurance application campaign.

The campaign workspace is deliberately outside the production crates. The
fixture is a small runnable v1 of the incremental build/publication platform;
the model receives the upgrade task and later correction messages. The script
does not modify the repository checkout or print provider credentials.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
CAMPAIGN = REPO / "target" / "runtime-endurance-v1" / "incremental-platform-20260919"
WORK = CAMPAIGN / "workspace"


FILES: dict[str, str] = {
    "TASK.md": """# Incremental Build and Publication Platform v1

This is a deliberately small runnable v1. The next task will upgrade it across
the modules below. Do not modify TASK.md, tests, fixtures or oracle.py.

The v1 contract is intentionally incomplete: it supports a basic JSONL import,
a small DAG evaluator, local CAS blobs, one SQLite queue and a local publication
stub. Upgrade requests will add strict schemas, content fingerprints, worker
fencing, atomic manifests, an outbox, migration and recovery documentation.
""",
    "app/__init__.py": """\"\"\"Incremental data platform fixture.\"\"\"\n""",
    "app/engine.py": """\nimport hashlib\nimport json\nfrom pathlib import Path\n\n\ndef read_jsonl(path: str | Path):\n    rows = []\n    for line in Path(path).read_text(encoding=\"utf-8\").splitlines():\n        if line.strip():\n            rows.append(json.loads(line))\n    return rows\n\n\ndef digest(value):\n    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False).encode()).hexdigest()\n\n\ndef execute_plan(rows, plan):\n    values = {\"input\": rows}\n    for node in plan:\n        op = node[\"op\"]\n        src = values[node.get(\"input\", \"input\")]\n        if op == \"select\":\n            values[node[\"id\"]] = [{k: row[k] for k in node[\"columns\"] if k in row} for row in src]\n        elif op == \"filter\":\n            values[node[\"id\"]] = [row for row in src if row.get(node[\"key\"]) == node[\"value\"]]\n        elif op == \"rename\":\n            values[node[\"id\"]] = [{node.get(\"mapping\", {}).get(k, k): v for k, v in row.items()} for row in src]\n        elif op == \"sort\":\n            values[node[\"id\"]] = sorted(src, key=lambda row: row.get(node[\"key\"]))\n        elif op == \"export\":\n            values[node[\"id\"]] = src\n        else:\n            raise ValueError(f\"unsupported operation: {op}\")\n    return values[plan[-1][\"id\"]] if plan else rows\n\n\ndef write_json(path, rows):\n    Path(path).write_text(json.dumps(rows, ensure_ascii=False, sort_keys=True, indent=2) + \"\\n\", encoding=\"utf-8\")\n""",
    "app/store.py": """\nimport hashlib\nfrom pathlib import Path\n\n\nclass CasStore:\n    def __init__(self, root):\n        self.root = Path(root)\n        self.root.mkdir(parents=True, exist_ok=True)\n\n    def put(self, data: bytes):\n        digest = hashlib.sha256(data).hexdigest()\n        target = self.root / digest\n        if not target.exists():\n            temp = target.with_suffix(\".tmp\")\n            temp.write_bytes(data)\n            temp.replace(target)\n        return digest\n\n    def get(self, digest):\n        return (self.root / digest).read_bytes()\n\n    def gc(self, referenced):\n        removed = []\n        for path in self.root.iterdir():\n            if path.is_file() and path.name not in referenced and not path.name.endswith(\".tmp\"):\n                path.unlink()\n                removed.append(path.name)\n        return removed\n""",
    "app/queue.py": """\nimport sqlite3\nimport uuid\n\n\nclass JobQueue:\n    def __init__(self, path):\n        self.db = sqlite3.connect(path, timeout=30, isolation_level=None)\n        self.db.execute(\"PRAGMA journal_mode=WAL\")\n        self.db.execute(\"CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, state TEXT, payload TEXT, token TEXT)\")\n\n    def close(self):\n        self.db.close()\n\n    def enqueue(self, job_id, payload):\n        self.db.execute(\"INSERT OR IGNORE INTO jobs VALUES(?,?,?,NULL)\", (job_id, \"queued\", payload))\n\n    def claim(self):\n        self.db.execute(\"BEGIN IMMEDIATE\")\n        row = self.db.execute(\"SELECT id,payload FROM jobs WHERE state='queued' ORDER BY id LIMIT 1\").fetchone()\n        if row is None:\n            self.db.execute(\"COMMIT\")\n            return None\n        token = uuid.uuid4().hex\n        self.db.execute(\"UPDATE jobs SET state='running', token=? WHERE id=?\", (token, row[0]))\n        self.db.execute(\"COMMIT\")\n        return {\"id\": row[0], \"payload\": row[1], \"token\": token}\n\n    def ack(self, job_id, token):\n        cur = self.db.execute(\"UPDATE jobs SET state='succeeded' WHERE id=? AND state='running' AND token=?\", (job_id, token))\n        return cur.rowcount == 1\n""",
    "app/publish.py": """\nimport json\nimport sqlite3\nimport urllib.request\n\n\nclass Outbox:\n    def __init__(self, path):\n        self.db = sqlite3.connect(path)\n        self.db.execute(\"CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY, body TEXT, status TEXT)\")\n        self.db.commit()\n\n    def add(self, identity, body):\n        self.db.execute(\"INSERT OR IGNORE INTO outbox VALUES(?,?,?)\", (identity, json.dumps(body, sort_keys=True), \"pending\"))\n        self.db.commit()\n\n    def pending(self):\n        return self.db.execute(\"SELECT id,body FROM outbox WHERE status='pending' ORDER BY id\").fetchall()\n\n    def close(self):\n        self.db.close()\n\n\ndef publish(url, identity, body):\n    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers={\"Content-Type\": \"application/json\", \"Idempotency-Key\": identity})\n    with urllib.request.urlopen(request, timeout=5) as response:\n        return response.read()\n""",
    "app/migrate.py": """\nimport sqlite3\n\n\ndef ensure_v1(path):\n    db = sqlite3.connect(path)\n    version = db.execute(\"PRAGMA user_version\").fetchone()[0]\n    if version == 0:\n        db.execute(\"CREATE TABLE IF NOT EXISTS manifests(id TEXT PRIMARY KEY, digest TEXT NOT NULL)\")\n        db.execute(\"PRAGMA user_version=1\")\n        db.commit()\n    elif version != 1:\n        raise ValueError(f\"unsupported schema version {version}\")\n    db.close()\n""",
    "app/cli.py": """\nimport argparse\nimport json\nfrom .engine import execute_plan, read_jsonl, write_json\n\n\ndef main(argv=None):\n    parser = argparse.ArgumentParser()\n    parser.add_argument(\"--input\")\n    parser.add_argument(\"--plan\")\n    parser.add_argument(\"--output\")\n    args = parser.parse_args(argv)\n    rows = read_jsonl(args.input)\n    plan = json.loads(open(args.plan, encoding=\"utf-8\").read()) if args.plan else []\n    write_json(args.output, execute_plan(rows, plan))\n    return 0\n\n\nif __name__ == \"__main__\":\n    raise SystemExit(main())\n""",
    "fixtures/input.jsonl": "{\"id\":1,\"kind\":\"a\",\"amount\":10}\n{\"id\":2,\"kind\":\"b\",\"amount\":20}\n{\"id\":3,\"kind\":\"a\",\"amount\":30}\n",
    "fixtures/plan.json": "[{\"id\":\"select\",\"op\":\"select\",\"columns\":[\"id\",\"kind\",\"amount\"]},{\"id\":\"only-a\",\"op\":\"filter\",\"input\":\"select\",\"key\":\"kind\",\"value\":\"a\"},{\"id\":\"sorted\",\"op\":\"sort\",\"input\":\"only-a\",\"key\":\"amount\"},{\"id\":\"export\",\"op\":\"export\",\"input\":\"sorted\"}]\n",
    "tests/test_platform.py": """\nimport json\nimport tempfile\nimport unittest\nfrom pathlib import Path\nfrom app.engine import execute_plan, read_jsonl\nfrom app.queue import JobQueue\nfrom app.store import CasStore\n\n\nclass PlatformV1(unittest.TestCase):\n    def setUp(self):\n        self.root = Path(tempfile.mkdtemp())\n\n    def test_engine_and_cas_are_deterministic(self):\n        rows = read_jsonl(Path(__file__).parents[1] / \"fixtures/input.jsonl\")\n        plan = json.loads((Path(__file__).parents[1] / \"fixtures/plan.json\").read_text())\n        value = execute_plan(rows, plan)\n        self.assertEqual([row[\"amount\"] for row in value], [10, 30])\n        store = CasStore(self.root / \"cas\")\n        digest = store.put(json.dumps(value, sort_keys=True).encode())\n        self.assertEqual(store.put(json.dumps(value, sort_keys=True).encode()), digest)\n        self.assertEqual(store.get(digest), json.dumps(value, sort_keys=True).encode())\n\n    def test_queue_fences_old_token(self):\n        queue = JobQueue(self.root / \"jobs.sqlite\")\n        queue.enqueue(\"a\", \"payload\")\n        job = queue.claim()\n        self.assertTrue(queue.ack(\"a\", job[\"token\"]))\n        self.assertFalse(queue.ack(\"a\", job[\"token\"]))\n        queue.close()\n""",
    "oracle.py": """\nimport json\nfrom pathlib import Path\n\ndef expected():\n    rows=[json.loads(line) for line in (Path(__file__).parent/'fixtures/input.jsonl').read_text().splitlines() if line]\n    return sorted([r for r in rows if r['kind']=='a'], key=lambda r:r['amount'])\n\nif __name__=='__main__':\n    print(json.dumps(expected(), sort_keys=True))\n""",
}


def write_file(relative: str, content: str) -> None:
    path = WORK / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def setup() -> None:
    WORK.mkdir(parents=True, exist_ok=True)
    for relative, content in FILES.items():
        write_file(relative, content)
    (CAMPAIGN / "baseline-lock.json").write_text(
        json.dumps(
            {
                "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
                "fixture_sha256": {
                    relative: hashlib.sha256(content.encode()).hexdigest()
                    for relative, content in FILES.items()
                    if relative.startswith("fixtures/") or relative == "TASK.md"
                },
                "runtime_binary_sha256": hashlib.sha256((REPO / "target/debug/agent-tui.exe").read_bytes()).hexdigest(),
                "provider_attempts": 0,
                "status": "PREPARED",
            },
            indent=2,
        ),
        encoding="utf-8",
    )
    print(json.dumps({"campaign": str(CAMPAIGN), "workspace": str(WORK), "files": len(FILES)}))


def l0() -> None:
    setup()
    commands = [
        ["python", "-m", "unittest", "discover", "-s", "tests", "-v"],
        ["python", "oracle.py"],
    ]
    results = []
    for command in commands:
        result = subprocess.run(command, cwd=WORK, capture_output=True, text=True, timeout=120)
        results.append({"command": command, "exit": result.returncode, "stdout_tail": result.stdout[-2000:], "stderr_tail": result.stderr[-1000:]})
    (CAMPAIGN / "l0-receipt.json").write_text(json.dumps({"results": results}, indent=2), encoding="utf-8")
    print(json.dumps({"l0": results}, ensure_ascii=False))
    if any(result["exit"] != 0 for result in results):
        raise SystemExit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["setup", "l0"])
    args = parser.parse_args()
    (setup if args.command == "setup" else l0)()


if __name__ == "__main__":
    main()

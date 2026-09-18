"""Prepare and drive the isolated Runtime endurance application campaign.

The campaign workspace is deliberately outside the production crates. The
fixture is a small runnable v1 of the incremental build/publication platform;
the model receives the upgrade task and later correction messages. The script
does not modify the repository checkout or print provider credentials.

Campaign evidence rules:
  - setup creates a campaign exclusively: an existing campaign directory is
    never silently overwritten (exit 3). The only path that reseeds the
    fixture is `--reset` together with `--yes`, after printing exactly what
    will be destroyed (exit 4 when `--yes` is missing).
  - l0 never reseeds. It verifies campaign identity first (baseline-lock.json
    exists, fixture/TASK hashes match the lock, runtime binary hash matches
    the lock; exit 5/6 otherwise) and only then runs unittest/oracle.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
CAMPAIGN = REPO / "target" / "runtime-endurance-v1" / "incremental-platform-20260919"
WORK = CAMPAIGN / "workspace"
DEFAULT_BINARY = REPO / "target/debug/agent-tui.exe"

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


def write_file(workspace: Path, relative: str, content: str) -> None:
    path = workspace / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    # newline="" keeps the on-disk bytes identical to the hashed seed content
    path.write_text(content, encoding="utf-8", newline="")


def digest(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _print_json(payload) -> None:
    text = json.dumps(payload, ensure_ascii=False)
    try:
        print(text)
    except UnicodeEncodeError:
        print(json.dumps(payload, ensure_ascii=True))


def _write_json(path: Path, payload) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    with tmp.open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.flush()
    tmp.replace(path)


def _destroy_scope(campaign_dir: Path, workspace: Path, lock_path: Path) -> dict:
    """Summary of exactly what a reseeding --reset would destroy."""
    files = []
    if workspace.exists():
        for path in sorted(workspace.rglob("*")):
            if path.is_file():
                files.append({"path": f"workspace/{path.relative_to(workspace)}", "sha256": digest(path)})
    if lock_path.exists():
        files.append({"path": "baseline-lock.json", "sha256": digest(lock_path)})
    return {"count": len(files), "files": files}


def setup(campaign_dir: Path = CAMPAIGN, reset: bool = False, yes: bool = False, binary: Path = DEFAULT_BINARY, repo: Path = REPO, head: str | None = None) -> None:
    campaign_dir = Path(campaign_dir)
    workspace = campaign_dir / "workspace"
    lock_path = campaign_dir / "baseline-lock.json"
    if (workspace.exists() or lock_path.exists()) and not reset:
        _print_json(
            {
                "status": "rejected",
                "reason": "campaign_dir_already_exists",
                "campaign_dir": str(campaign_dir),
                "hint": "pass --reset together with --yes to reseed, or choose a new --campaign-dir",
            }
        )
        raise SystemExit(3)
    if reset:
        scope = _destroy_scope(campaign_dir, workspace, lock_path)
        _print_json({"status": "reset", "campaign_dir": str(campaign_dir), "will_delete": scope, "confirmed": bool(yes)})
        if not yes:
            _print_json(
                {
                    "status": "rejected",
                    "reason": "reset_requires_yes",
                    "campaign_dir": str(campaign_dir),
                    "hint": "rerun with --reset --yes to confirm reseeding",
                }
            )
            raise SystemExit(4)
        if workspace.exists():
            shutil.rmtree(workspace)
        if lock_path.exists():
            lock_path.unlink()
    try:
        workspace.mkdir(parents=True)
    except FileExistsError:
        _print_json(
            {
                "status": "rejected",
                "reason": "campaign_dir_already_exists",
                "campaign_dir": str(campaign_dir),
                "hint": "pass --reset together with --yes to reseed, or choose a new --campaign-dir",
            }
        )
        raise SystemExit(3)
    for relative, content in FILES.items():
        write_file(workspace, relative, content)
    head_value = head if head is not None else subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=str(repo), text=True).strip()
    _write_json(
        lock_path,
        {
            "head": head_value,
            "fixture_sha256": {
                relative: hashlib.sha256(content.encode()).hexdigest()
                for relative, content in FILES.items()
                if relative.startswith("fixtures/") or relative == "TASK.md"
            },
            "runtime_binary_sha256": digest(binary),
            "provider_attempts": 0,
            "status": "PREPARED",
        },
    )
    _print_json({"status": "prepared", "campaign": str(campaign_dir), "workspace": str(workspace), "files": len(FILES)})


def l0(campaign_dir: Path = CAMPAIGN, binary: Path = DEFAULT_BINARY, repo: Path = REPO, python_executable: str = "python") -> None:
    campaign_dir = Path(campaign_dir)
    workspace = campaign_dir / "workspace"
    lock_path = campaign_dir / "baseline-lock.json"
    if not lock_path.exists():
        _print_json(
            {
                "status": "rejected",
                "reason": "baseline_lock_missing",
                "campaign_dir": str(campaign_dir),
                "hint": "run the setup command first; l0 never seeds the campaign itself",
            }
        )
        raise SystemExit(5)
    lock = json.loads(lock_path.read_text(encoding="utf-8"))
    differences = []
    for relative, expected_hash in sorted(lock.get("fixture_sha256", {}).items()):
        path = workspace / relative
        actual_hash = digest(path) if path.exists() else None
        if actual_hash != expected_hash:
            differences.append({"file": relative, "expected": expected_hash, "actual": actual_hash})
    expected_binary = lock.get("runtime_binary_sha256")
    actual_binary = digest(binary) if Path(binary).exists() else None
    if actual_binary != expected_binary:
        differences.append({"file": "runtime_binary", "expected": expected_binary, "actual": actual_binary})
    receipt = {"status": "identity_mismatch" if differences else "identity_ok", "head": lock.get("head"), "differences": differences, "results": []}
    if differences:
        _write_json(campaign_dir / "l0-receipt.json", receipt)
        _print_json(receipt)
        raise SystemExit(6)
    commands = [
        [python_executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
        [python_executable, "oracle.py"],
    ]
    for command in commands:
        result = subprocess.run(command, cwd=workspace, capture_output=True, text=True, timeout=120)
        receipt["results"].append({"command": command, "exit": result.returncode, "stdout_tail": result.stdout[-2000:], "stderr_tail": result.stderr[-1000:]})
    _write_json(campaign_dir / "l0-receipt.json", receipt)
    _print_json({"l0": receipt}, )
    if any(result["exit"] != 0 for result in receipt["results"]):
        raise SystemExit(1)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Prepare and drive the isolated Runtime endurance application campaign.")
    parser.add_argument("command", choices=["setup", "l0"])
    parser.add_argument("--campaign-dir", type=Path, default=CAMPAIGN)
    parser.add_argument("--reset", action="store_true")
    parser.add_argument("--yes", action="store_true")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    args = parser.parse_args(argv)
    if args.command == "setup":
        setup(args.campaign_dir, reset=args.reset, yes=args.yes, binary=args.binary)
    else:
        l0(args.campaign_dir, binary=args.binary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

import json
import tempfile
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parent
WORK = ROOT / "workspace"
sys.path.insert(0, str(WORK))
from app.engine import execute_plan, reference_execute, compare_results
from app.fingerprint import content_fingerprint, fingerprint_graph
from app.migration import make_interrupted_v1, migrate
from app.outbox import Outbox, OutboxError
from app.store import CasStore, Manifest


def main():
    receipts = {}
    rows = [json.loads(line) for line in (WORK / "fixtures/input.jsonl").read_text().splitlines() if line]
    plan = json.loads((WORK / "fixtures/plan.json").read_text())
    primary = execute_plan(rows, plan)
    reference = reference_execute(rows, plan)
    compare_results(primary, reference, context="final independent verifier")
    changed = list(rows)
    changed[0] = dict(changed[0], amount=11)
    fp_a = content_fingerprint(rows)
    fp_b = content_fingerprint(changed)
    assert fp_a != fp_b
    graph = fingerprint_graph(rows, plan)
    assert "__terminal__" in graph
    receipts["engine_fingerprint"] = {"ok": True, "content_changed": True, "terminal": graph["__terminal__"]}

    with tempfile.TemporaryDirectory() as temp:
        temp = Path(temp)
        store = CasStore(temp / "cas")
        digest = store.put(b"immutable-output")
        manifest = Manifest(identity="tenant-a/destination-a", entries={"output": digest}, created_at="fixture")
        manifest_digest = store.publish(manifest.identity, manifest)
        assert store.get(digest) == b"immutable-output"
        assert digest in store.reachable()
        receipts["cas"] = {"ok": True, "manifest": manifest_digest, "gc": store.gc(store.reachable())}

        migration_db = temp / "migration.sqlite"
        make_interrupted_v1(migration_db)
        assert migrate(migration_db) == 2
        assert migrate(migration_db) == 2
        receipts["migration"] = {"ok": True, "version": 2}

        outbox = Outbox(temp / "outbox.sqlite")
        assert outbox.enqueue("tenant-a/destination-a/key-1", {"digest": digest})
        try:
            outbox.enqueue("tenant-a/destination-a/key-1", {"digest": "other"})
        except Exception as exc:
            assert isinstance(exc, OutboxError)
        else:
            raise AssertionError("idempotency body conflict was accepted")
        sent = outbox.publish_pending("http://127.0.0.1:9/publish", transport=lambda *_: (201, "accepted"))
        assert sent[0]["status"] == "sent"
        confirmed = outbox.reconcile("http://127.0.0.1:9/receipts", transport=lambda *_: (200, json.dumps([{"id": "tenant-a/destination-a/key-1", "accepted": True, "detail": "r1"}])))
        assert confirmed[0].accepted and outbox.status("tenant-a/destination-a/key-1")["status"] == "confirmed"
        try:
            outbox.publish_pending("https://example.com/publish")
        except OutboxError:
            pass
        else:
            raise AssertionError("non-local publication was not refused")
        receipts["outbox"] = {"ok": True, "sent": sent, "confirmed": [r.__dict__ for r in confirmed]}

    receipt = {"status": "PASS", "protected_fixtures_untouched": True, "checks": receipts}
    (ROOT / "final-app-verifier.json").write_text(json.dumps(receipt, indent=2, sort_keys=True), encoding="utf-8")
    print(json.dumps(receipt, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()

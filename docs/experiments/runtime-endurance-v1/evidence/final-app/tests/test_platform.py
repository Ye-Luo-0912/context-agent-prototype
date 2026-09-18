
import json
import tempfile
import unittest
from pathlib import Path
from app.engine import execute_plan, read_jsonl
from app.queue import JobQueue
from app.store import CasStore


class PlatformV1(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())

    def test_engine_and_cas_are_deterministic(self):
        rows = read_jsonl(Path(__file__).parents[1] / "fixtures/input.jsonl")
        plan = json.loads((Path(__file__).parents[1] / "fixtures/plan.json").read_text())
        value = execute_plan(rows, plan)
        self.assertEqual([row["amount"] for row in value], [10, 30])
        store = CasStore(self.root / "cas")
        digest = store.put(json.dumps(value, sort_keys=True).encode())
        self.assertEqual(store.put(json.dumps(value, sort_keys=True).encode()), digest)
        self.assertEqual(store.get(digest), json.dumps(value, sort_keys=True).encode())

    def test_queue_fences_old_token(self):
        queue = JobQueue(self.root / "jobs.sqlite")
        queue.enqueue("a", "payload")
        job = queue.claim()
        self.assertTrue(queue.ack("a", job["token"]))
        self.assertFalse(queue.ack("a", job["token"]))
        queue.close()

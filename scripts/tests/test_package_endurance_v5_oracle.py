"""Oracle invariant regression for package_endurance_v5 (review F06/F07).

Runs the repository-side invariant battery, which is derived from the review's
counterexample script but reads the frozen bounds from the oracle itself, and
closes every SQLite handle so it also runs on Windows.
"""
import json
from pathlib import Path
import sys
import unittest

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
REPO = SCRIPTS.parent
V5 = SCRIPTS / "package_endurance_v5"
for path in (str(TESTS), str(V5)):
    sys.path.insert(0, path)

import probe_oracle_invariants as probe  # noqa: E402
from _v5_modules import import_v5  # noqa: E402

oracle = import_v5("oracle")


class InvariantBattery(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.report = probe.run(REPO)

    def test_no_invariant_leaks(self):
        self.assertEqual(self.report["leaked"], 0, self.report["findings"])

    def test_every_planned_counterexample_is_rejected(self):
        rejected = {row["id"]: row["rejected"] for row in self.report["findings"]}
        for identifier in ("O1", "O2", "O3", "O4", "O5", "O6", "O7", "O8", "O9", "O10"):
            self.assertTrue(rejected[identifier], f"{identifier} leaked: {rejected}")

    def test_positive_control_is_still_accepted(self):
        control = next(row for row in self.report["findings"] if row["id"] == "P1")
        self.assertTrue(control["rejected"] is False or control["leaked"] is False)
        self.assertIsNone(control["error"])
        self.assertGreater(self.report["control_summary"]["receipts"], 0)

    def test_wal_is_visible_in_the_frozen_reading_mode(self):
        counts = self.report["wal_visibility"]["receipt_counts"]
        self.assertTrue(self.report["wal_visibility"]["wal_exists"])
        self.assertGreater(counts["mode=ro"], counts["mode=ro&immutable=1"])
        self.assertEqual(self.report["wal_visibility"]["frozen_reading_mode"], "mode=ro")


class OracleUnits(unittest.TestCase):
    def test_generation_gap_is_rejected_even_without_the_probe_battery(self):
        import tempfile
        with tempfile.TemporaryDirectory() as raw:
            base = Path(raw)
            root = base / "gap"
            probe.make_repository(root, oracle)
            import sqlite3
            connection = sqlite3.connect(root / "repo.sqlite")
            try:
                connection.execute("DELETE FROM receipts WHERE key='r2'")
                connection.commit()
            finally:
                connection.close()
            with self.assertRaises(ValueError):
                oracle.repository_cut(root)

    def test_read_transaction_leaves_no_sidecar_and_no_mutation(self):
        import tempfile
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "quiescent"
            probe.make_repository(root, oracle)
            before = oracle.source_fingerprint(root)
            oracle.repository_cut(root)
            oracle.repository_cut(root)
            self.assertEqual(oracle.source_fingerprint(root), before)
            self.assertFalse((root / "repo.sqlite-journal").exists())

    def test_fingerprint_detects_a_write(self):
        import tempfile
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "fingerprint"
            probe.make_repository(root, oracle)
            before = oracle.source_fingerprint(root)
            (root / "unexpected.bin").write_bytes(b"x")
            self.assertNotEqual(oracle.source_fingerprint(root), before)


if __name__ == "__main__":
    unittest.main()

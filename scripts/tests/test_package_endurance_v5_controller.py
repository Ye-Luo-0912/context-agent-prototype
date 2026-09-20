"""Continuous-load controller regression for V5 (review F03/F04/F05/F08).

The controller runs against the real v3 fixture application and the real
independent oracle, with the candidate entry points supplied in-process by a
test reference implementation. No provider call, no Rust Runtime.
"""
import json
from pathlib import Path
import sys
import tempfile
import unittest

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
V5 = SCRIPTS / "package_endurance_v5"
for path in (str(TESTS), str(V5)):
    sys.path.insert(0, path)

from _v5_modules import import_v5  # noqa: E402
from _v5_reference import dispatcher, make_workspace  # noqa: E402

continuous_load = import_v5("continuous_load")
oracle = import_v5("oracle")
workload = import_v5("workload")


def make_stage(root: Path) -> Path:
    stage = Path(root) / "campaign"
    stage.mkdir(parents=True)
    (stage / "campaign.json").write_text(json.dumps(dict(
        schema=1, kind="v5_online_backup_continuous", target_load_seconds=7200,
        install_budget=workload.INSTALL_BUDGET, writer_workers=workload.WRITER_WORKERS,
        backup_every=workload.BACKUP_EVERY)), encoding="utf-8")
    make_workspace(stage / "workspace", packages=workload.CATALOG_PACKAGES,
                   versions=workload.CATALOG_VERSIONS)
    return stage


class ControllerLoad(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.stage = make_stage(self.root)
        self.original = continuous_load.candidate_process

    def tearDown(self):
        continuous_load.candidate_process = self.original

    def load(self, **kwargs):
        continuous_load.candidate_process = self.candidate
        options = dict(seconds=3, seed=20260921, out_name="continuous-load", window=1,
                       writer_workers=2, backup_every=2, period_floor=0.005,
                       required_seconds=0.3)
        options.update(kwargs)
        exit_code = continuous_load.run(self.stage, options.pop("seconds"), options.pop("seed"), **options)
        receipt = json.loads((self.stage / (kwargs.get("out_name") or "continuous-load") /
                              "receipt.json").read_bytes())
        return exit_code, receipt

    def test_window_passes_with_measured_concurrency(self):
        reference = dispatcher()
        self.candidate = reference
        exit_code, receipt = self.load()
        self.assertEqual(receipt["status"], "PASS", receipt["reasons"])
        self.assertEqual(exit_code, 0)
        overlap = receipt["overlap"]
        self.assertGreater(overlap["writer_writer"]["pairs"], 0, overlap)
        self.assertGreater(overlap["writer_writer"]["longest_seconds"], 0)
        self.assertGreater(overlap["verify_writer"]["pairs"], 0, overlap)
        self.assertGreater(overlap["gc_writer"]["pairs"], 0, overlap)
        self.assertGreaterEqual(receipt["archives"], 1)
        self.assertGreaterEqual(receipt["verified"], 1)
        self.assertEqual(receipt["fault_coverage"]["observed"], 1)
        self.assertEqual(receipt["unresolved_failures"], [])
        window = receipt["acceptance_windows"][-1]
        self.assertEqual(window["verdict"], "PASS", window)
        self.assertEqual(window["faults"], dict(planned=1, observed=1))
        self.assertGreaterEqual(window["passed"], 1)

    def test_quiesced_cut_window_is_recorded_and_stable(self):
        self.candidate = dispatcher()
        _, receipt = self.load()
        events = [json.loads(line) for line in
                  (self.stage / "workspace/runtime-feedback/events.jsonl")
                  .read_text(encoding="utf-8").splitlines()]
        archive_rows = [row for row in events if row.get("kind") == "archive-cycle"]
        self.assertTrue(archive_rows)
        for row in archive_rows:
            evidence = row["cut_evidence"]
            self.assertTrue(evidence["stable"])
            self.assertEqual(evidence["before"], evidence["after"])
            self.assertFalse(evidence["quiesce"]["active"])
            self.assertTrue(evidence["quiesce"]["exclusive"])
        self.assertEqual(receipt["refused_batches"], 0)

    def test_candidate_failure_stays_visible_while_other_batches_succeed(self):
        # Slot 1 is always tenant-01/prod, so this obligation fails early and
        # never resolves: the other scopes keep succeeding meanwhile.
        self.candidate = dispatcher(failing=dict(
            install=lambda kwargs: (kwargs["tenant"], kwargs["environment"]) == ("tenant-01", "prod")))
        _, receipt = self.load()
        self.assertNotEqual(receipt["status"], "PASS")
        self.assertGreaterEqual(receipt["failure_summary"]["total"], 1)
        obligations = {row["obligation"] for row in receipt["unresolved_failures"]}
        self.assertIn("install", obligations)
        row = next(item for item in receipt["unresolved_failures"] if item["obligation"] == "install")
        self.assertEqual(row["scope"], "tenant-01/prod")
        self.assertIn("slot=", row["reproduce"])
        self.assertGreaterEqual(row["count"], 2)
        feedback = json.loads((self.stage / "workspace/runtime-feedback/latest.json").read_bytes())
        self.assertIn("unresolved_failures", feedback)
        self.assertEqual(feedback["failures_open"], len(feedback["unresolved_failures"]))
        self.assertTrue(any(item["obligation"] == "install" for item in feedback["unresolved_failures"]))
        # ordinary batches of other scopes kept running and are recorded as such
        self.assertGreater(receipt["batches"], 0)
        self.assertGreater(receipt["archives"], 0)
        self.assertGreater(receipt["overlap"]["writer_writer"]["pairs"], 0)

    def test_unobserved_crash_boundary_is_never_reported_as_triggered(self):
        self.candidate = dispatcher(honor_crash=False)
        _, receipt = self.load()
        fault = receipt["fault_ledger"][0]
        self.assertEqual(fault["verdict"], "NOT_TRIGGERED")
        self.assertFalse(fault["fired"])
        self.assertFalse(fault["observed"])
        obligations = {row["obligation"] for row in receipt["unresolved_failures"]}
        self.assertIn("crash_boundary", obligations)

    def test_resume_uses_authority_generation_and_new_keys(self):
        self.candidate = dispatcher()
        _, first = self.load(seconds=2, required_seconds=0.2)
        repository = self.stage / "continuous-load" / "repository"
        before = continuous_load.authority_state(repository)
        self.assertGreater(before["max_slot"], 0)
        self.candidate = dispatcher()
        _, second = self.load(seconds=2, out_name="continuous-load-resume-01", window=2,
                              root_override=repository, resume=True, required_seconds=0.2)
        after = continuous_load.authority_state(repository)
        self.assertGreater(after["max_slot"], before["max_slot"])
        evidence = json.loads((self.stage / "continuous-load-resume-01/resume-evidence.json").read_bytes())
        self.assertTrue(evidence["resume"])
        self.assertGreaterEqual(evidence["start_slot"], before["max_slot"] + 1)
        self.assertEqual(evidence["authority"]["generations"], before["generations"])
        self.assertGreaterEqual(after["receipt_count"], before["receipt_count"])
        # Contiguous generations after a resume: no old key was replayed as new load.
        descriptor, members = oracle.repository_cut(repository)
        self.assertGreater(oracle.summary(descriptor, members)["receipts"], before["receipt_count"] - 1)
        self.assertEqual(first["status"] in ("PASS", "INCOMPLETE"), True)
        self.assertIn(second["status"], ("PASS", "INCOMPLETE"))


class FailureProjection(unittest.TestCase):
    """A matching success clears an obligation; ordinary batches never do."""

    def test_other_batches_never_clear_an_unresolved_failure(self):
        ledger = continuous_load.FailureLedger()
        ledger.fail("install", slot=5, scope="tenant-00/staging", operation="install",
                    detail="wal refusal", reproduce="invoke.py install slot=5", digest="d1")
        self.assertFalse(ledger.resolve("install", scope="tenant-00/prod", operation="install"))
        self.assertFalse(ledger.resolve("backup", scope="tenant-00/staging", operation="install"))
        self.assertEqual(len(ledger.rows()), 1)
        self.assertTrue(ledger.resolve("install", scope="tenant-00/staging", operation="install"))
        self.assertEqual(ledger.rows(), [])

    def test_projection_is_bounded_and_reports_what_it_dropped(self):
        ledger = continuous_load.FailureLedger(limit=2)
        for index in range(4):
            ledger.fail(f"obligation-{index}", slot=index, scope="s", operation="op",
                        detail="d", reproduce="r", digest="d1")
        self.assertEqual(len(ledger.rows()), 2)
        self.assertEqual(ledger.summary()["dropped_for_capacity"], 2)
        self.assertEqual(ledger.summary()["total"], 4)


class ReplayReduction(unittest.TestCase):
    """An early failure never blocks a later frozen version's own window."""

    def test_failures_survive_the_switch_and_the_new_window_opens(self):
        windows = continuous_load.AcceptanceWindows(required_seconds=1.0)
        windows.observe("digest-a", now=0.0, outcome="failed")
        windows.observe("digest-a", now=2.0, outcome="failed")
        windows.observe("digest-b", now=3.0, outcome="passed")
        windows.observe("digest-b", now=5.0, outcome="passed")
        windows.close(5.0)
        history = windows.history()
        self.assertEqual(len(history), 2)
        self.assertEqual(history[0]["candidate_digest"], "digest-a")
        self.assertEqual(history[0]["failed"], 2)
        self.assertEqual(history[0]["verdict"], "INCOMPLETE")
        self.assertEqual(history[1]["candidate_digest"], "digest-b")
        self.assertEqual(history[1]["verdict"], "PASS")
        self.assertEqual(history[1]["reasons"], [])

    def test_short_window_is_incomplete_even_when_every_cycle_passed(self):
        windows = continuous_load.AcceptanceWindows(required_seconds=120.0)
        windows.observe("digest-a", now=0.0, outcome="passed")
        finished = windows.close(5.0)
        self.assertEqual(finished["verdict"], "INCOMPLETE")
        self.assertTrue(any("required" in reason for reason in finished["reasons"]))


if __name__ == "__main__":
    unittest.main()

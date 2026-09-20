"""Budget, authorization and derived-result regression for V5 (review F09/F10/F11)."""
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
V5 = SCRIPTS / "package_endurance_v5"
sys.path.insert(0, str(TESTS))
sys.path.insert(0, str(SCRIPTS))

from _v5_modules import import_v5  # noqa: E402

campaign_accounting = import_v5("campaign_accounting")
finalize = import_v5("finalize")
runner_grants = import_v5("runner_grants")
v5_run = import_v5("run")
import runtime_endurance_incremental_runner as runner  # noqa: E402


def caps(**overrides):
    value = dict(kind="v5_online_backup_continuous", created_epoch=1_000_000.0,
                 deadline_epoch=1_000_000.0 + 6 * 3600, main_decisions=260,
                 provider_attempts=300, tool_attempts=900, target_load_seconds=7200,
                 estimated_cost_usd=2.0, install_budget=4096)
    value.update(overrides)
    return value


class ReservationPolicy(unittest.TestCase):
    def test_declared_policy_matches_the_executed_amount(self):
        ledger = v5_run.V5Ledger(Path("ledger.json"),
                                 {"attempts": []}, runner.Pricing(), 8192, 2.0)
        policy = ledger._reserve_policy()
        self.assertEqual(policy["strategy"], "wire_bytes_plus_8192_and_full_output")
        self.assertIn("request_body_bytes + 8192", policy["input_estimate"])
        amount = ledger.estimate_reserve_usd(1200)
        expected = ((1200 + 8192) * ledger.pricing.input_per_mtoken_usd
                    + 8192 * ledger.pricing.output_per_mtoken_usd) / 1_000_000
        self.assertAlmostEqual(amount, expected)

    def test_default_ledger_keeps_its_historical_policy(self):
        ledger = runner.BudgetLedger(Path("ledger.json"), {"attempts": []},
                                     runner.Pricing(), 8192, 2.0)
        policy = ledger._reserve_policy()
        self.assertIn("request_body_bytes/4 + 256", policy["input_estimate"])
        self.assertAlmostEqual(ledger.estimate_reserve_usd(1200),
                               runner.Pricing().reserve_estimate_usd(1200, 8192))


class CrossSegmentAccounting(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name)
        self.stage.mkdir(exist_ok=True)
        (self.stage / "campaign.json").write_text(json.dumps(caps()), encoding="utf-8")

    def write_segment(self, name, rounds, tools):
        out = self.stage / name
        out.mkdir()
        (out / "summary.json").write_text(json.dumps(dict(outcome="completed", rounds=rounds,
                                                          tool_calls=tools, exit_code=0)),
                                          encoding="utf-8")

    def test_remaining_decisions_come_from_the_materials_not_the_caller(self):
        self.write_segment("model-continuous", 41, 76)
        self.write_segment("model-continuous-repair-01", 219, 319)
        ledger = dict(attempts=[dict(id=index + 1, status="committed", request=index + 1)
                                for index in range(262)])
        (self.stage / "budget-ledger.json").write_text(json.dumps(ledger), encoding="utf-8")
        allowance = v5_run.remaining_allowance(self.stage)
        self.assertEqual(allowance["accounting"]["main_decisions"], 260)
        self.assertEqual(allowance["accounting"]["tool_attempts"], 395)
        self.assertEqual(allowance["accounting"]["provider_attempts"], 262)
        self.assertEqual(allowance["main_decisions"], 0)
        self.assertEqual(allowance["provider_attempts"], 38)

    def test_exhausted_decision_budget_refuses_a_new_segment(self):
        self.write_segment("model-continuous", 260, 100)
        with self.assertRaises(ValueError):
            v5_run.run(self.stage, rounds=10)

    def test_unreadable_segment_is_reported_instead_of_counted_as_zero(self):
        out = self.stage / "model-continuous"
        out.mkdir()
        (out / "summary.json").write_text("{not json", encoding="utf-8")
        accounting = campaign_accounting.campaign_accounting(self.stage)
        self.assertEqual(accounting["incomplete_segments"], ["model-continuous"])


class Grants(unittest.TestCase):
    def test_frozen_campaign_grants_are_compatible(self):
        grants = runner_grants.campaign_grants(caps(), python=sys.executable, now=1_000_000.0)
        report = runner_grants.compatibility(caps(), grants, now=1_000_000.0)
        self.assertTrue(report["compatible"], report["problems"])
        self.assertGreaterEqual(report["granted"]["max_runs"], 260 * 8)

    def test_scoreboard_default_grants_are_refused_for_a_two_hour_task(self):
        short = [
            dict(id="app-write", risk="WorkspaceWrite",
                 target={"workspace_path_prefix": "app"},
                 constraint={"max_content_bytes": 160000},
                 expires_at_ms=int((1_000_000 + 1800) * 1000)),
            dict(id="python-tests", risk="ProcessExecution",
                 target={"exec_argv_prefix": [sys.executable]},
                 constraint={"max_runs": 48},
                 expires_at_ms=int((1_000_000 + 1800) * 1000)),
        ]
        report = runner_grants.compatibility(caps(), short, now=1_000_000.0)
        self.assertFalse(report["compatible"])
        self.assertTrue(any("below the required" in problem for problem in report["problems"]))
        self.assertTrue(any("grant expiry leaves" in problem for problem in report["problems"]))

    def test_grant_is_bounded_by_the_campaign_deadline(self):
        grants = runner_grants.campaign_grants(caps(), python=sys.executable, now=1_000_000.0)
        for grant in grants:
            self.assertLessEqual(grant["expires_at_ms"], int(caps()["deadline_epoch"] * 1000))


class DerivedResults(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name) / "campaign"
        self.stage.mkdir(parents=True)
        (self.stage / "campaign.json").write_text(json.dumps(caps()), encoding="utf-8")
        (self.stage / "baseline-lock.json").write_text(json.dumps(dict(head="abc", files={})),
                                                       encoding="utf-8")
        (self.stage / "contract-identity.json").write_text(json.dumps(dict(files={}, workspace={})),
                                                           encoding="utf-8")
        (self.stage / "capacity-plan.json").write_text(json.dumps(dict(satisfiable=True)),
                                                       encoding="utf-8")
        (self.stage / "workspace").mkdir()

    def write_window(self, name, status, reasons=()):
        window = self.stage / name
        window.mkdir()
        (window / "receipt.json").write_text(json.dumps(dict(status=status, reasons=list(reasons),
                                                             stopped_by="duration")), encoding="utf-8")

    def test_missing_materials_make_the_receipt_incomplete(self):
        (self.stage / "contract-identity.json").unlink()
        with self.assertRaises(finalize.MissingMaterial):
            finalize.main(self.stage, Path(self.temp.name) / "out")

    def test_receipt_is_derived_from_materials_and_reports_missing_windows(self):
        output = Path(self.temp.name) / "frozen"
        finalize.main(self.stage, output)
        frozen = json.loads((output / "FINAL_STATUS.json").read_bytes())
        self.assertEqual(frozen["status"], "INCOMPLETE_NO_CONTROLLER_OUTPUT")
        self.assertEqual(frozen["segments"], [])
        self.assertEqual(frozen["controller_windows"], [])
        self.assertEqual(frozen["candidate"]["present"], False)
        self.assertEqual(frozen["candidate_local_tests"]["status"], "NOT_RECORDED")
        self.assertIn("FINAL_STATUS.json", json.loads((output / "MANIFEST.json").read_bytes())["files"])

    def test_status_is_derived_from_the_last_controller_window(self):
        self.write_window("continuous-load", "INCOMPLETE", ["candidate refused every backup"])
        segments = [dict(segment="model-continuous", rounds=41)]
        status, reasons = finalize.derive_status(True, finalize.controller_receipts(self.stage), segments)
        self.assertEqual(status, "NOT_ACCEPTED_DURATION")
        self.assertEqual(reasons, ["candidate refused every backup"])

    def test_passing_window_is_accepted_and_protected_change_is_rejected(self):
        self.write_window("continuous-load", "PASS")
        segments = [dict(segment="model-continuous", rounds=41)]
        status, reasons = finalize.derive_status(True, finalize.controller_receipts(self.stage), segments)
        self.assertEqual(status, "ACCEPTED")
        self.assertEqual(reasons, [])
        status, _ = finalize.derive_status(False, finalize.controller_receipts(self.stage), segments)
        self.assertEqual(status, "REJECTED_PROTECTED_FILES_CHANGED")

    def test_controller_only_stage_is_never_accepted(self):
        self.write_window("continuous-load", "PASS")
        status, reasons = finalize.derive_status(True, finalize.controller_receipts(self.stage), [])
        self.assertEqual(status, "INCOMPLETE_NO_MODEL_SEGMENT")
        self.assertTrue(reasons)

    def test_unreadable_segments_are_not_derived_as_a_pass(self):
        self.write_window("continuous-load", "PASS")
        segments = [dict(segment="model-continuous", rounds=None)]
        status, _ = finalize.derive_status(True, finalize.controller_receipts(self.stage), segments)
        self.assertEqual(status, "INCOMPLETE_UNREADABLE_SEGMENTS")

    def test_absent_controller_output_is_incomplete_not_success(self):
        status, reasons = finalize.derive_status(True, [], [])
        self.assertEqual(status, "INCOMPLETE_NO_CONTROLLER_OUTPUT")
        self.assertTrue(reasons)

    def test_candidate_tests_are_discovered_or_reported_missing(self):
        self.assertEqual(finalize.candidate_tests(self.stage)["status"], "NOT_RECORDED")
        segment = self.stage / "model-continuous"
        segment.mkdir()
        (segment / "model-tests.log").write_text("Ran 27 tests\nERROR: one\nOK\nexit code 1\n",
                                                  encoding="utf-8")
        report = finalize.candidate_tests(self.stage)
        self.assertEqual(report["status"], "RECORDED")
        self.assertEqual((report["total"], report["errors"]), (27, 1))


class FrozenToolBudgetWiring(unittest.TestCase):
    """The frozen campaign tool budget reaches the segment, with the baseline
    read back from the campaign's own materials (review F10 residual)."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.stage = Path(self.temp.name) / "campaign"
        self.stage.mkdir(parents=True)
        now = time.time()
        self.caps = caps(created_epoch=now, deadline_epoch=now + 6 * 3600)
        (self.stage / "campaign.json").write_text(json.dumps(self.caps), encoding="utf-8")
        (self.stage / "baseline-lock.json").write_text(
            json.dumps(dict(head="abc", files={"app/repository.py": "hash"})), encoding="utf-8")
        (self.stage / "workspace").mkdir()
        prior = self.stage / "model-continuous"
        prior.mkdir()
        (prior / "summary.json").write_text(
            json.dumps(dict(outcome="completed", rounds=5, tool_calls=700, exit_code=0)),
            encoding="utf-8")

    def test_campaign_tool_budget_and_materials_baseline_reach_the_segment(self):
        captured = {}
        original_popen = v5_run.subprocess.Popen
        original_segment = v5_run.run_segment

        class FakeLoader:
            def __init__(self, *args, **kwargs):
                self.returncode = 0
                self.pid = 4242

            def poll(self):
                return 0

            def terminate(self):
                raise AssertionError("an already-exited loader must not be terminated")

            def wait(self, timeout=None):
                return 0

            def kill(self):
                raise AssertionError("an already-exited loader must not be killed")

        v5_run.subprocess.Popen = lambda *args, **kwargs: FakeLoader()
        v5_run.run_segment = lambda config: captured.setdefault("config", config) and 0
        try:
            self.assertEqual(v5_run.run(self.stage, rounds=1, load_wait_seconds=0), 0)
        finally:
            v5_run.subprocess.Popen = original_popen
            v5_run.run_segment = original_segment
        config = captured["config"]
        self.assertEqual(config.tool_budget, self.caps["tool_attempts"])
        self.assertEqual(config.tool_budget_baseline, 700,
                         "the baseline comes from the campaign materials, not from the caller")
        self.assertEqual(config.rounds, 1)
        self.assertTrue(config.grants, "the campaign grant plan is handed to the segment")
        handoff = json.loads((self.stage / "segment-handoff.json").read_bytes())
        self.assertEqual(handoff["tool_budget"], dict(cap=self.caps["tool_attempts"], baseline=700))
        self.assertEqual(handoff["accounting"]["tool_attempts"], 700)


if __name__ == "__main__":
    unittest.main()

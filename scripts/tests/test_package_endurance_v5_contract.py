"""Contract and capacity regression for the frozen V5 workload (review F01/F02)."""
import json
import sys
import unittest
from pathlib import Path

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
V5 = SCRIPTS / "package_endurance_v5"
sys.path.insert(0, str(TESTS))

from _v5_modules import import_v5  # noqa: E402

oracle = import_v5("oracle")
workload = import_v5("workload")
preflight = import_v5("preflight")


class FrozenContract(unittest.TestCase):
    def test_contract_text_states_the_bounds_the_oracle_enforces(self):
        text = (V5 / "SPEC.md").read_text(encoding="utf-8")
        bounds = workload.check_spec_bounds(text)
        self.assertEqual(bounds["mismatches"], [], bounds)
        self.assertTrue(bounds["agrees"])

    def test_contract_no_longer_demands_immutable_reads(self):
        text = (V5 / "SPEC.md").read_text(encoding="utf-8")
        # The review's F01: the contract demanded immutable while the judge read
        # with mode=ro. The frozen contract must name one mode for both.
        self.assertIn("mode=ro", text)
        for line in text.splitlines():
            if "immutable" in line:
                self.assertIn("must not be used", line)

    def test_oracle_never_opens_an_immutable_uri(self):
        source = (V5 / "oracle.py").read_text(encoding="utf-8")
        self.assertIn("?mode=ro", source)
        self.assertNotIn("immutable=1\"", source)

    def test_contract_requires_a_read_transaction_and_quiesced_window(self):
        text = (V5 / "SPEC.md").read_text(encoding="utf-8")
        self.assertIn("read transaction", text)
        self.assertIn("quiesced", text)


class CapacityPlan(unittest.TestCase):
    def test_frozen_workload_fits_every_frozen_bound(self):
        plan = workload.closure_plan()
        self.assertTrue(plan["satisfiable"], plan["exceeds"])
        for key, value in (("members", plan["members"]), ("member_bytes", plan["descriptor_member_bytes"]),
                           ("uncompressed_bytes", plan["uncompressed_bytes"]),
                           ("archive_bytes", plan["archive_bytes"])):
            self.assertLessEqual(value, plan["bounds"][key], key)

    def test_plan_matches_the_audit_arithmetic(self):
        # The review computed 463 members for the old 256 bound with 357 objects,
        # 91 manifests, 13 pointers, one descriptor and one outbox member.
        old_style = 357 + 91 + 13 + 1 + 1
        self.assertGreater(old_style, 256)
        self.assertGreater(workload.closure_plan()["bounds"]["members"], old_style)

    def test_oversized_workload_is_refused_before_a_window_opens(self):
        plan = workload.closure_plan(install_budget=workload.INSTALL_BUDGET * 8)
        self.assertFalse(plan["satisfiable"])
        with self.assertRaises(ValueError):
            workload.assert_satisfiable(plan)

    def test_window_budget_is_cumulative_and_bounded(self):
        self.assertEqual(workload.window_budget(1), workload.INSTALL_BUDGET_PER_WINDOW)
        self.assertEqual(workload.window_budget(workload.MAX_WINDOWS), workload.INSTALL_BUDGET)
        with self.assertRaises(ValueError):
            workload.window_budget(workload.MAX_WINDOWS + 1)

    def test_install_budget_is_derived_from_the_member_byte_bound(self):
        plan = workload.closure_plan()
        self.assertLess(plan["descriptor_member_bytes"], oracle.MEMBER_BYTE_BOUND)
        # The member bound is the binding constraint, not just the count.
        self.assertLess(oracle.MEMBER_BYTE_BOUND - plan["descriptor_member_bytes"],
                        oracle.MEMBER_BYTE_BOUND * 0.5)


class Gates(unittest.TestCase):
    def test_preflight_refuses_when_the_recorded_plan_drifts(self):
        import tempfile
        with tempfile.TemporaryDirectory() as raw:
            stage = Path(raw)
            caps = dict(kind="v5_online_backup_continuous", install_budget=workload.INSTALL_BUDGET,
                        writer_workers=workload.WRITER_WORKERS)
            (stage / "campaign.json").write_text(json.dumps(caps), encoding="utf-8")
            drifted = workload.closure_plan()
            drifted["install_budget"] = 7
            (stage / "capacity-plan.json").write_text(json.dumps(drifted), encoding="utf-8")
            with self.assertRaises(ValueError):
                preflight.capacity_check(stage, caps)

    def test_preflight_accepts_the_frozen_plan(self):
        import tempfile
        with tempfile.TemporaryDirectory() as raw:
            stage = Path(raw)
            caps = dict(kind="v5_online_backup_continuous", install_budget=workload.INSTALL_BUDGET,
                        writer_workers=workload.WRITER_WORKERS)
            (stage / "campaign.json").write_text(json.dumps(caps), encoding="utf-8")
            (stage / "capacity-plan.json").write_text(json.dumps(workload.closure_plan()), encoding="utf-8")
            plan = preflight.capacity_check(stage, caps)
            self.assertTrue(plan["satisfiable"])


if __name__ == "__main__":
    unittest.main()

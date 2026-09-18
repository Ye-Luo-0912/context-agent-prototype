"""BR7 regressions: campaign exclusive creation, guarded --reset, identity checks."""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

import _support as support

campaign = support.campaign_mod
runner = support.runner


def hash_tree(root: Path) -> dict[str, str]:
    return {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


class CampaignGuardTests(unittest.TestCase):
    def _setup(self, camp: Path, binary: Path) -> None:
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            campaign.setup(camp, binary=binary)

    def test_second_setup_rejected_without_touching_files(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            binary = root / "stub-binary.bin"
            binary.write_bytes(b"binary-v1")
            self._setup(camp, binary)
            drifted = camp / "workspace" / "app" / "engine.py"
            drifted.write_text("# drifted\n", encoding="utf-8")
            before = hash_tree(camp)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                with self.assertRaises(SystemExit) as ctx:
                    campaign.setup(camp, binary=binary)
            self.assertEqual(ctx.exception.code, 3)
            self.assertEqual(hash_tree(camp), before, "second setup must not change a single byte")
            self.assertEqual(drifted.read_text(encoding="utf-8"), "# drifted\n")
            status = json.loads(stdout.getvalue().strip().splitlines()[-1])
            self.assertEqual(status["status"], "rejected")

    def test_reset_requires_yes_and_reseeds_only_with_yes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            binary = root / "stub-binary.bin"
            binary.write_bytes(b"binary-v1")
            self._setup(camp, binary)
            drifted = camp / "workspace" / "app" / "engine.py"
            drifted.write_text("# drifted\n", encoding="utf-8")
            before = hash_tree(camp)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                with self.assertRaises(SystemExit) as ctx:
                    campaign.setup(camp, binary=binary, reset=True)
            self.assertEqual(ctx.exception.code, 4, "--reset without --yes must refuse and destroy nothing")
            self.assertEqual(hash_tree(camp), before)
            with contextlib.redirect_stdout(stdout):
                campaign.setup(camp, binary=binary, reset=True, yes=True)
            self.assertEqual(
                drifted.read_text(encoding="utf-8"),
                campaign.FILES["app/engine.py"],
                "--reset --yes is the only path that reseeds",
            )
            lock = support.read_json(camp / "baseline-lock.json")
            for relative, content in campaign.FILES.items():
                if relative.startswith("fixtures/") or relative == "TASK.md":
                    expected = hashlib.sha256(content.encode()).hexdigest()
                    self.assertEqual(lock["fixture_sha256"][relative], expected)

    def test_l0_identity_mismatch_refuses(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            binary_a = root / "binary-a.bin"
            binary_a.write_bytes(b"binary-a")
            binary_b = root / "binary-b.bin"
            binary_b.write_bytes(b"binary-b")
            self._setup(camp, binary_a)
            fixture = camp / "workspace" / "fixtures" / "input.jsonl"
            fixture.write_text('{"id": 999}\n', encoding="utf-8")
            with self.assertRaises(SystemExit) as ctx:
                campaign.l0(camp, binary=binary_a, python_executable=sys.executable)
            self.assertEqual(ctx.exception.code, 6)
            receipt = support.read_json(camp / "l0-receipt.json")
            self.assertEqual(receipt["status"], "identity_mismatch")
            self.assertTrue(receipt["differences"])
            fixture.write_text(campaign.FILES["fixtures/input.jsonl"], encoding="utf-8")
            with self.assertRaises(SystemExit) as ctx:
                campaign.l0(camp, binary=binary_b, python_executable=sys.executable)
            self.assertEqual(ctx.exception.code, 6, "runtime binary hash mismatch must refuse too")

    def test_l0_identity_ok_runs_checks(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            binary = root / "stub-binary.bin"
            binary.write_bytes(b"binary-v1")
            self._setup(camp, binary)
            campaign.l0(camp, binary=binary, python_executable=sys.executable)
            receipt = support.read_json(camp / "l0-receipt.json")
            self.assertEqual(receipt["status"], "identity_ok")
            self.assertEqual([result["exit"] for result in receipt["results"]], [0, 0])

    def test_l0_without_baseline_lock_refuses(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            camp.mkdir()
            with self.assertRaises(SystemExit) as ctx:
                campaign.l0(camp, binary=root / "whatever.bin", python_executable=sys.executable)
            self.assertEqual(ctx.exception.code, 5)

    def test_runner_resume_identity_mismatch_refused(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = support.make_campaign(root / "campaign")
            exit0 = support.stub_file(root, "exit0.py", support.CHILD_EXIT_0)
            first = support.base_config(camp, child_command=[sys.executable, exit0])
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run_segment(first), 0)
            second = support.base_config(
                camp,
                segment="seg-002",
                child_command=[sys.executable, exit0],
                identity_head="fedcba9876543210fedcba9876543210fedcba98",
            )
            ledger_before = support.read_json(camp / "budget-ledger.json")
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_segment(second)
            self.assertEqual(code, runner.EXIT_IDENTITY_MISMATCH)
            self.assertFalse((camp / "seg-002").exists(), "identity refusal must not start a segment")
            status = json.loads(stdout.getvalue().strip().splitlines()[-1])
            self.assertEqual(status["status"], "identity_mismatch")
            ledger_after = support.read_json(camp / "budget-ledger.json")
            self.assertEqual(
                ledger_after["attempts"],
                ledger_before["attempts"],
                "no paid attempt may be accepted after identity refusal",
            )
            refusals = list((camp / "refusals").glob("seg-002-*.json"))
            self.assertTrue(refusals)

    def test_campaign_cli_setup_refuses_second_time(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            camp = root / "camp"
            binary = root / "stub-binary.bin"
            binary.write_bytes(b"binary-v1")
            self.assertEqual(campaign.main(["setup", "--campaign-dir", str(camp), "--binary", str(binary)]), 0)
            with self.assertRaises(SystemExit) as ctx:
                campaign.main(["setup", "--campaign-dir", str(camp), "--binary", str(binary)])
            self.assertEqual(ctx.exception.code, 3)
            self.assertEqual(campaign.main(["l0", "--campaign-dir", str(camp), "--binary", str(binary)]), 0)


if __name__ == "__main__":
    unittest.main()

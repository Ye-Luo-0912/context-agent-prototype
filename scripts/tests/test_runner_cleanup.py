"""BR5 regressions: unconditional finalization, receipt preservation, exit layering.

All scenarios use stub child processes (python -c / tiny scripts) and never
read eval.env or contact a real provider.
"""
from __future__ import annotations

import io
import contextlib
import json
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

import _support as support

runner = support.runner


class RunnerCleanupTests(unittest.TestCase):
    def test_communicate_timeout_terminates_child_and_writes_receipt(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            cfg = support.base_config(
                campaign,
                child_command=[sys.executable, "-c", "import time; time.sleep(60)"],
                child_wait_timeout_s=1.0,
                child_graceful_timeout_s=0.8,
                child_kill_timeout_s=8.0,
            )
            code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_CHILD_TIMEOUT)
            out = campaign / "seg-001"
            metadata = support.read_json(out / "metadata.json")
            self.assertEqual(metadata["outcome"], "child_wait_timeout")
            self.assertIn("child_wait_timeout", metadata["categories"])
            # terminal receipts all landed even though the child never exited
            for name in ("usage-ledger.json", "summary.json", "pid.txt", "stdout.log", "stderr.log"):
                self.assertTrue((out / name).exists(), name)
            cleanup = metadata["cleanup"]["child"]
            self.assertEqual(cleanup["outcome"], "terminated_confirmed")
            pid = int((out / "pid.txt").read_text())
            self.assertFalse(support.pid_alive(pid), "child process must be reaped, not orphaned")

    def test_keyboard_interrupt_shares_finalization_and_marks_receipt(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            interrupt_event = threading.Event()
            cfg = support.base_config(
                campaign,
                child_command=[sys.executable, "-c", "import time; time.sleep(60)"],
                child_wait_timeout_s=60.0,
                child_graceful_timeout_s=0.8,
                child_kill_timeout_s=8.0,
                interrupt_event=interrupt_event,
            )
            result = {}

            def work():
                result["code"] = runner.run_segment(cfg)

            thread = threading.Thread(target=work)
            thread.start()
            pid_path = campaign / "seg-001" / "pid.txt"
            deadline = time.time() + 10
            while not pid_path.exists() and time.time() < deadline:
                time.sleep(0.02)
            self.assertTrue(pid_path.exists(), "child should have started")
            time.sleep(0.2)
            interrupt_event.set()
            thread.join(timeout=30)
            self.assertFalse(thread.is_alive())
            self.assertEqual(result["code"], runner.EXIT_INTERRUPTED)
            out = campaign / "seg-001"
            metadata = support.read_json(out / "metadata.json")
            self.assertEqual(metadata["outcome"], "interrupted")
            self.assertIn("interrupted", metadata["categories"])
            self.assertEqual(metadata["cleanup"]["child"]["outcome"], "terminated_confirmed")
            self.assertTrue((out / "usage-ledger.json").exists())
            self.assertTrue((out / "summary.json").exists())
            pid = int((out / "pid.txt").read_text())
            self.assertFalse(support.pid_alive(pid), "no orphan process after KeyboardInterrupt")

    def test_child_launch_failure_reports_clear_status(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            cfg = support.base_config(campaign, child_command=[str(root / "missing-binary.exe")])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_LAUNCH_FAILED)
            out = campaign / "seg-001"
            metadata = support.read_json(out / "metadata.json")
            self.assertEqual(metadata["outcome"], "launch_failed")
            self.assertIn("child_launch_failed", metadata["categories"])
            self.assertTrue(metadata.get("launch_error"))
            for name in ("usage-ledger.json", "summary.json", "stdout.log", "stderr.log"):
                self.assertTrue((out / name).exists(), name)

    def test_normal_completion_exit_zero_full_receipt(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            cfg = support.base_config(campaign, child_command=[sys.executable, support.stub_file(root, "exit0.py", support.CHILD_EXIT_0)])
            code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            out = campaign / "seg-001"
            metadata = support.read_json(out / "metadata.json")
            summary = support.read_json(out / "summary.json")
            self.assertEqual(metadata["outcome"], "completed")
            self.assertEqual(metadata["exit"], 0)
            self.assertEqual(metadata["child_exit_code"], 0)
            self.assertEqual(metadata["categories"], [])
            self.assertEqual(summary["outcome"], "completed")
            self.assertEqual(summary["exit_code"], 0)
            self.assertEqual(summary["categories"], [])
            self.assertTrue(summary["protected_unchanged"])
            self.assertIn("cleanup", summary)
            self.assertIn("budget", summary)
            self.assertTrue((out / "usage-ledger.json").exists())

    def test_child_nonzero_maps_to_runner_exit_and_preserves_code(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            cfg = support.base_config(campaign, child_command=[sys.executable, support.stub_file(root, "exit7.py", support.CHILD_EXIT_7)])
            code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_CHILD_FAILED)
            metadata = support.read_json(campaign / "seg-001" / "metadata.json")
            self.assertEqual(metadata["child_exit_code"], 7, "child's own exit code must be preserved verbatim")
            self.assertIn("child_nonzero", metadata["categories"])

    def test_protected_modification_maps_to_protection_exit(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            tamper = support.stub_file(root, "tamper.py", support.CHILD_TAMPER)
            cfg = support.base_config(
                campaign,
                child_command=[sys.executable, tamper, str(campaign / "workspace" / "TASK.md")],
            )
            code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_PROTECTED_MODIFIED)
            metadata = support.read_json(campaign / "seg-001" / "metadata.json")
            self.assertFalse(metadata["protected_unchanged"])
            self.assertIn("protected_modified", metadata["categories"])
            self.assertEqual(metadata["child_exit_code"], 0)

    def test_segment_dir_exists_refused_without_touching_it(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            out = campaign / "seg-001"
            out.mkdir(parents=False)
            sentinel = out / "sentinel.txt"
            sentinel.write_text("previous segment evidence", encoding="utf-8")
            cfg = support.base_config(campaign, child_command=[sys.executable, support.stub_file(root, "exit0.py", support.CHILD_EXIT_0)])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_SEGMENT_EXISTS)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "previous segment evidence")
            status = json.loads(stdout.getvalue().strip().splitlines()[-1])
            self.assertEqual(status["status"], "segment_exists")
            refusals = list((campaign / "refusals").glob("seg-001-*.json"))
            self.assertTrue(refusals, "refusal receipt must be recorded")


if __name__ == "__main__":
    unittest.main()

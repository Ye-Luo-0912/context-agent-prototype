"""Campaign admission and owned process tree regressions; no provider calls."""
from __future__ import annotations

import contextlib
import ctypes
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import _support as support

runner = support.runner


class RunnerBaselineTests(unittest.TestCase):
    @staticmethod
    def baseline(campaign):
        files = {name: hashlib.sha256((campaign / "workspace" / name).read_bytes()).hexdigest()
                 for name in support.PROTECTED_FILES}
        (campaign / "baseline-lock.json").write_text(json.dumps({
            "head": support.HEAD,
            "runtime_binary_sha256": hashlib.sha256(Path(sys.executable).read_bytes()).hexdigest(),
            "files": files,
        }), encoding="utf-8")
        return files

    def test_baseline_drift_refused_before_child_or_provider_admission(self):
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / "campaign")
            path = campaign / "workspace" / "TASK.md"
            original = hashlib.sha256(path.read_bytes()).hexdigest()
            self.baseline(campaign)
            path.write_text("changed between segments", encoding="utf-8")
            cfg = support.base_config(campaign, child_command=[sys.executable, "-c", "pass"])
            with mock.patch.object(runner, "_spawn_child", wraps=runner._spawn_child) as spawn, \
                    mock.patch.object(runner, "_Relay", wraps=runner._Relay) as relay, \
                    contextlib.redirect_stdout(io.StringIO()):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_IDENTITY_MISMATCH)
            spawn.assert_not_called()
            relay.assert_not_called()
            self.assertFalse((campaign / "budget-ledger.json").exists())
            self.assertFalse((campaign / "seg-001").exists())
            receipt = support.read_json(next((campaign / "refusals").glob("seg-001-*.json")))
            self.assertEqual(receipt["status"], "identity_mismatch")
            self.assertIn(original, json.dumps(receipt))

    def test_valid_baseline_allows_candidate_edits_outside_protected_set(self):
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / "campaign")
            self.baseline(campaign)
            app = campaign / "workspace" / "app.py"
            app.write_text("old candidate", encoding="utf-8")
            cfg = support.base_config(campaign, child_command=[sys.executable, "-c", "pass"])
            app.write_text("repaired candidate", encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run_segment(cfg), runner.EXIT_OK)

    def test_missing_or_unrecorded_protected_file_is_not_a_valid_baseline(self):
        for defect in ("missing_file", "missing_digest", "invalid_manifest"):
            with self.subTest(defect=defect), tempfile.TemporaryDirectory() as raw:
                campaign = support.make_campaign(Path(raw) / "campaign")
                self.baseline(campaign)
                lock = campaign / "baseline-lock.json"
                if defect == "missing_file":
                    (campaign / "workspace" / "TASK.md").unlink()
                else:
                    data = json.loads(lock.read_text())
                    if defect == "missing_digest":
                        del data["files"]["TASK.md"]
                    else:
                        data["files"] = []
                    lock.write_text(json.dumps(data), encoding="utf-8")
                cfg = support.base_config(campaign, child_command=[sys.executable, "-c", "pass"])
                with mock.patch.object(runner, "_spawn_child") as spawn, contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(runner.run_segment(cfg), runner.EXIT_IDENTITY_MISMATCH)
                spawn.assert_not_called()


@unittest.skipUnless(os.name == "nt", "real Windows job ownership regression")
class RunnerWindowsTreeTests(unittest.TestCase):
    def test_failed_launch_cleanup_retains_process_and_fences_receipt(self):
        import runner_process_tree as process_tree
        child = mock.Mock(pid=12345, returncode=None, stdin=None, stdout=None, stderr=None)
        child.poll.return_value = None
        child.kill.side_effect = OSError("injected direct kill failure")
        child.wait.side_effect = subprocess.TimeoutExpired("injected child", 0)
        job = mock.Mock(handle=1)
        job.assign.side_effect = OSError("injected assignment failure")
        job.close.side_effect = OSError("injected Job close failure")
        final_cleanup = {"outcome": "unconfirmed", "tree_confirmed": False,
                         "detail": "injected final observation failure"}
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / "campaign")
            cfg = support.base_config(campaign, child_command=[sys.executable, "-c", "pass"])
            with mock.patch.object(process_tree, "WindowsJob", return_value=job), \
                    mock.patch.object(process_tree.subprocess, "Popen", return_value=child), \
                    mock.patch.object(runner, "_stop_child", return_value=final_cleanup) as stop, \
                    contextlib.redirect_stdout(io.StringIO()):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_CLEANUP_UNCONFIRMED)
            stop.assert_called_once_with(child, cfg.child_graceful_timeout_s, cfg.child_kill_timeout_s)
            child.wait.assert_called_once()
            job.close.assert_called_once()
            receipt = support.read_json(campaign / "seg-001" / "metadata.json")
            self.assertIn("child_launch_failed", receipt["categories"])
            self.assertIn("cleanup_unconfirmed", receipt["categories"])
            self.assertEqual(receipt["cleanup"]["child"]["outcome"], "unconfirmed")
            launch_cleanup = receipt["cleanup"]["launch"]
            self.assertEqual(launch_cleanup["outcome"], "unconfirmed")
            for detail in ("direct kill failure", "TimeoutExpired", "Job close failure"):
                self.assertIn(detail, json.dumps(launch_cleanup))
            self.assertIs(child._runner_job, job)

    def test_empty_job_waits_for_retained_direct_process_handle(self):
        import runner_process_tree as process_tree
        child = process_tree.spawn_windows_owned(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            # Model the observed native race: Job accounting reaches zero
            # while a nonblocking poll has not yet observed process exit.
            # Both Job termination and wait() still use real Windows handles.
            with mock.patch.object(child, "poll", return_value=None):
                result = process_tree.stop_windows_owned(child, 0, 5)
            self.assertEqual(result["active_processes"], 0)
            self.assertTrue(result["tree_confirmed"], result)
            self.assertEqual(result["outcome"], "terminated_confirmed")
            self.assertIsNotNone(child.poll())
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            child._runner_job.close()

    def test_job_assignment_failure_never_runs_candidate(self):
        import runner_process_tree as process_tree
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            marker = root / "must-not-run"
            cfg = support.base_config(campaign, child_command=[
                sys.executable, "-c", "import sys; from pathlib import Path; Path(sys.argv[1]).write_text('ran')", str(marker),
            ])
            original_spawn = subprocess.Popen
            processes = []
            def capture_spawn(*args, **kwargs):
                child = original_spawn(*args, **kwargs)
                processes.append(child)
                return child
            with mock.patch.object(process_tree.WindowsJob, "assign", side_effect=OSError("assignment refused")), \
                    mock.patch.object(process_tree.subprocess, "Popen", side_effect=capture_spawn) as spawn, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run_segment(cfg), runner.EXIT_LAUNCH_FAILED)
            self.assertEqual(spawn.call_count, 1)
            self.assertFalse(marker.exists(), "candidate must remain suspended until ownership succeeds")
            self.assertIsNotNone(processes[0].poll(), "failed suspended launch must be reaped")
            cleanup = support.read_json(campaign / "seg-001" / "metadata.json")["cleanup"]
            self.assertNotIn("child", cleanup, "failed suspended launch was never returned to the runner")

    def test_unowned_injected_process_is_never_cleanup_confirmed(self):
        with subprocess.Popen([sys.executable, "-c", "pass"], stdin=subprocess.DEVNULL,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) as child:
            child.wait(timeout=5)
            result = runner._stop_child(child, 0, 1)
        self.assertEqual(result["outcome"], "unconfirmed")
        self.assertFalse(result["tree_confirmed"])
        self.assertEqual(result["scope"], "unowned_process")

    def test_job_observation_failure_fences_final_receipt(self):
        import runner_process_tree as process_tree
        with tempfile.TemporaryDirectory() as raw:
            campaign = support.make_campaign(Path(raw) / "campaign")
            cfg = support.base_config(campaign, child_command=[sys.executable, "-c", "pass"])
            with mock.patch.object(process_tree.WindowsJob, "active_processes", side_effect=OSError("query refused")), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(runner.run_segment(cfg), runner.EXIT_CLEANUP_UNCONFIRMED)
            receipt = support.read_json(campaign / "seg-001" / "metadata.json")
            self.assertIn("cleanup_unconfirmed", receipt["categories"])
            self.assertEqual(receipt["cleanup"]["child"]["outcome"], "unconfirmed")

    def test_exited_parent_does_not_leave_live_descendant(self):
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        kernel.OpenProcess.restype = ctypes.c_void_p
        kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        kernel.WaitForSingleObject.restype = ctypes.c_ulong
        kernel.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint]
        kernel.CloseHandle.argtypes = [ctypes.c_void_p]
        kernel.IsProcessInJob.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.POINTER(ctypes.c_int)]
        kernel.IsProcessInJob.restype = ctypes.c_int
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            pidfile = root / "descendant.pid"
            child = (
                "import subprocess,sys; from pathlib import Path; "
                "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
                "stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); "
                "Path(sys.argv[1]).write_text(str(p.pid))"
            )
            cfg = support.base_config(campaign, child_command=[sys.executable, "-c", child, str(pidfile)])
            handle = None
            exited_at_cleanup_return = None
            original_stop = runner._stop_child
            def retain_identity_then_stop(process, *args):
                nonlocal handle, exited_at_cleanup_return
                self.assertTrue(pidfile.exists(), "descendant must actually have been launched")
                handle = kernel.OpenProcess(0x00100000 | 0x1000 | 0x0001, False, int(pidfile.read_text()))
                self.assertTrue(handle, "retain the original descendant before cleanup can recycle its PID")
                member = ctypes.c_int()
                self.assertTrue(kernel.IsProcessInJob(handle, process._runner_job.handle, ctypes.byref(member)))
                self.assertTrue(member.value, "the retained identity must belong to the launched Job")
                self.assertEqual(kernel.WaitForSingleObject(handle, 0), 258, "descendant must be live before cleanup")
                result = original_stop(process, *args)
                exited_at_cleanup_return = kernel.WaitForSingleObject(handle, 0) == 0
                return result
            try:
                with mock.patch.object(runner, "_stop_child", side_effect=retain_identity_then_stop), \
                        contextlib.redirect_stdout(io.StringIO()):
                    code = runner.run_segment(cfg)
                self.assertTrue(handle)
                cleanup = support.read_json(campaign / "seg-001" / "metadata.json")["cleanup"]["child"]
                self.assertTrue(exited_at_cleanup_return, f"retained descendant HANDLE is not signaled at cleanup return: {cleanup}")
                self.assertEqual(code, runner.EXIT_OK)
                self.assertEqual(cleanup["scope"], "windows_job")
                self.assertEqual(cleanup["active_processes"], 0)
                self.assertTrue(cleanup["tree_confirmed"])
            finally:
                if handle:
                    if kernel.WaitForSingleObject(handle, 0) == 258:
                        kernel.TerminateProcess(handle, 37)
                        kernel.WaitForSingleObject(handle, 5000)
                    kernel.CloseHandle(handle)


if __name__ == "__main__":
    unittest.main()

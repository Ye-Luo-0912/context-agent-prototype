"""Bounded lifetime identity capture on actual Windows children."""
import ctypes
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

import _support  # Registers the repository scripts directory for direct discovery.
import runner_process_tree as helper

PARENT=r'''import subprocess,sys,time
from pathlib import Path
root=Path(sys.argv[1]);(root/'parent-ready').touch()
while not (root/'spawn').exists():time.sleep(.001)
source="import sys,time;from pathlib import Path;p=Path(sys.argv[1]);\nwhile not (p/'child-exit').exists():time.sleep(.001)"
child=subprocess.Popen([sys.executable,'-c',source,str(root)],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
(root/'child-pid').write_text(str(child.pid))
child.wait();(root/'child-done').touch()
while not (root/'parent-exit').exists():time.sleep(.001)
'''


def wait_for(predicate):
    end=time.monotonic()+5
    while time.monotonic()<end:
        if predicate():return
        time.sleep(.001)
    raise TimeoutError('expected native lifecycle boundary was not observed')


def child_pid(root):
    try:return int((root/'child-pid').read_text())
    except (FileNotFoundError,ValueError):return None


@unittest.skipUnless(os.name == 'nt', 'native Windows Job lifetime observation')
class LifetimeTests(unittest.TestCase):
    def test_snapshot_retries_keep_original_deadline_and_attempt_bound(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        deadline = time.monotonic() + 5
        job.capture_member_handles = Mock(side_effect=helper.JobSnapshotChanged('members changed'))
        with self.assertRaises(helper.JobSnapshotChanged):
            job._capture_for_tracking(deadline)
        self.assertEqual(job.capture_member_handles.call_count, helper.MAX_SNAPSHOT_ATTEMPTS)
        self.assertTrue(all(call.args == (deadline,) for call in job.capture_member_handles.call_args_list))
        job.capture_member_handles.reset_mock()
        with patch.object(helper.time, 'monotonic', return_value=deadline):
            with self.assertRaises(helper.JobSnapshotChanged):
                job._capture_for_tracking(deadline)
        job.capture_member_handles.assert_called_once_with(deadline)

    def test_observer_keeps_collecting_after_one_changing_snapshot_pass(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        job._observer_stop = Mock()
        job._observer_stop.wait.side_effect = [False, False, True]
        job._observation_lock = threading.Lock()
        job._observation_error = None
        job._capture_for_tracking = Mock(side_effect=[helper.JobSnapshotChanged('members changed'), 2])
        job._observe_members()
        self.assertEqual(job._capture_for_tracking.call_count, 2)
        self.assertIsNone(job._observation_error)

    def test_native_capture_failure_is_not_retried(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        job.capture_member_handles = Mock(side_effect=OSError('native lookup failed'))
        with self.assertRaisesRegex(OSError, 'native lookup failed'):
            job._capture_for_tracking(time.monotonic() + 5)
        self.assertEqual(job.capture_member_handles.call_count, 1)

    def test_native_close_failure_is_not_downgraded_to_snapshot_churn(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        job.handle = 999
        job.accounting = Mock(side_effect=[
            SimpleNamespace(ActiveProcesses=1, TotalProcesses=1),
            SimpleNamespace(ActiveProcesses=2, TotalProcesses=2),
        ])

        def query(handle, kind, buffer, size, returned):
            row = buffer._obj
            row.assigned = row.count = 1
            row.ids[0] = 10
            return True

        def belongs(handle, job_handle, out):
            out._obj.value = True
            return True

        job.api = Mock(QueryInformationJobObject=query, IsProcessInJob=belongs)
        job.api.OpenProcess.return_value = 1010
        job.api.CloseHandle.return_value = False
        with self.assertRaises(OSError) as caught:
            job._capture_for_tracking(time.monotonic() + 5)
        self.assertNotIsInstance(caught.exception, helper.JobSnapshotChanged)
        self.assertEqual(job.accounting.call_count, 2)
        job.api.CloseHandle.assert_called_once_with(1010)

    def test_tracking_retries_real_capture_function_after_membership_growth(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        job.handle = 999
        job._observation_lock = threading.Lock()
        job._identities = {}
        job.accounting = Mock(side_effect=[
            SimpleNamespace(ActiveProcesses=1, TotalProcesses=1),
            SimpleNamespace(ActiveProcesses=2, TotalProcesses=2),
            SimpleNamespace(ActiveProcesses=2, TotalProcesses=2),
        ])
        capacities = []

        def query(handle, kind, buffer, size, returned):
            row = buffer._obj
            capacities.append(len(row.ids))
            row.assigned = 2
            row.count = min(2, len(row.ids))
            for index in range(row.count):
                row.ids[index] = 10 + index
            if len(row.ids) < 2:
                ctypes.set_last_error(234)
                return False
            return True

        def belongs(handle, job_handle, out):
            out._obj.value = True
            return True

        def times(handle, created, exited, kernel, user):
            created._obj.dwLowDateTime = handle
            return True

        job.api = Mock(QueryInformationJobObject=query,
                       OpenProcess=lambda access, inherit, pid: pid + 1000,
                       IsProcessInJob=belongs, GetProcessTimes=times,
                       GetProcessId=lambda handle: handle - 1000)
        deadline = time.monotonic() + 5
        with patch.object(job, 'capture_member_handles', wraps=job.capture_member_handles) as capture:
            self.assertEqual(job._capture_for_tracking(deadline), 2)
        self.assertEqual(capacities, [1, 2, 2])
        self.assertEqual([call.args for call in capture.call_args_list], [(deadline,), (deadline,)])
        self.assertEqual(job._identities, {(10, 1010): 1010, (11, 1011): 1011})

    def test_gated_native_member_growth_retains_both_exit_handles(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            process = self.spawn(root)
            job = process._runner_job
            job.stop_observer(time.monotonic() + 5)
            original_accounting = job.accounting
            initial_counts = []

            def grow_after_accounting():
                snapshot = original_accounting()
                if not initial_counts:
                    initial_counts.append(snapshot.ActiveProcesses)
                    (root / 'spawn').touch()
                    wait_for(lambda: child_pid(root) and original_accounting().ActiveProcesses == 2)
                return snapshot

            with patch.object(job, 'accounting', side_effect=grow_after_accounting):
                self.assertEqual(job._capture_for_tracking(time.monotonic() + 10), 2)
            self.assertEqual(initial_counts, [1])
            handles, error = job.retained_members()
            self.assertIsNone(error)
            self.assertEqual(len(handles), 2)
            self.assertTrue(all(job.api.WaitForSingleObject(handle, 0) == 258 for handle in handles))
            (root / 'child-exit').touch()
            wait_for(lambda: (root / 'child-done').exists())
            (root / 'parent-exit').touch()
            process.wait(timeout=5)
            self.assertTrue(all(job.api.WaitForSingleObject(handle, 0) == 0 for handle in handles))
            result = helper.stop_windows_owned(process, 0, 5)
            self.assertTrue(result['tree_confirmed'], result)
            self.assertEqual(result['observed_processes'], result['final_total_processes'])

    def test_close_failure_preserves_native_handles_for_retry(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        identity = (123, 456)
        job.handle = 100
        job._identities = {identity: 200}
        job._observation_lock = threading.Lock()
        job.stop_observer = Mock()
        job.api = Mock()
        job.api.CloseHandle.side_effect = [False, False, True, True]
        with self.assertRaises(OSError):
            job.close()
        self.assertEqual(job.handle, 100)
        self.assertEqual(job._identities, {identity: 200})
        job.close()
        self.assertIsNone(job.handle)
        self.assertEqual(job._identities, {})
        self.assertEqual([call.args[0] for call in job.api.CloseHandle.call_args_list], [200, 100, 200, 100])

    def test_final_cleanup_retries_members_after_job_handle_closed(self):
        job = helper.WindowsJob.__new__(helper.WindowsJob)
        job.handle = None
        job._identities = {(123, 456): 200}
        job._observation_lock = threading.Lock()
        job.stop_observer = Mock()
        job.api = Mock()
        job.api.CloseHandle.return_value = True
        process = Mock(_runner_job=job)
        process.poll.return_value = 0
        result = helper.stop_windows_owned(process, 0, 1)
        self.assertFalse(result['tree_confirmed'])
        self.assertEqual(job._identities, {})
        job.api.CloseHandle.assert_called_once_with(200)
        process.kill.assert_not_called()

    def spawn(self,root):
        process=helper.spawn_windows_owned([sys.executable,'-c',PARENT,str(root)],stdin=subprocess.DEVNULL,
                                          stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        self.addCleanup(self.close,process)
        wait_for(lambda:(root/'parent-ready').exists())
        return process

    @staticmethod
    def close(process):
        job=process._runner_job
        if job.handle:job.terminate()
        process.wait(timeout=5)
        job.close()

    @staticmethod
    def observed(job,pid):
        with job._observation_lock:return any(identity[0]==pid for identity in job._identities)

    def test_direct_process_enrolled_before_candidate_can_run(self):
        captured=[];original=helper.WindowsJob.resume
        def inspect(job,process):
            captured.append((self.observed(job,process.pid),len(job.retained_members()[0])))
            return original(job,process)
        with tempfile.TemporaryDirectory() as temp,patch.object(helper.WindowsJob,'resume',inspect):
            process=self.spawn(Path(temp))
        self.assertEqual(captured,[(True,1)])

    def test_completed_observed_child_history_can_be_confirmed(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);process=self.spawn(root);job=process._runner_job
            (root/'spawn').touch();wait_for(lambda:child_pid(root))
            wait_for(lambda:self.observed(job,child_pid(root)))
            (root/'child-exit').touch();wait_for(lambda:(root/'child-done').exists())
            (root/'parent-exit').touch();process.wait(timeout=5)
            handles=list(job.retained_members()[0]);self.assertEqual(len(handles),2)
            result=helper.stop_windows_owned(process,0,5)
            self.assertTrue(result['tree_confirmed'],result)
            self.assertEqual(result['observed_processes'],2)
            self.assertEqual(result['final_total_processes'],2)
            self.assertTrue(all(job.api.WaitForSingleObject(h,0)==0xffffffff for h in handles),'every retained handle must close')
            self.assertFalse(job._observer.is_alive())

    def test_missed_completed_child_history_remains_unconfirmed(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);process=self.spawn(root);job=process._runner_job
            job.stop_observer(time.monotonic()+5)
            (root/'spawn').touch();wait_for(lambda:child_pid(root))
            (root/'child-exit').touch();wait_for(lambda:(root/'child-done').exists())
            (root/'parent-exit').touch();process.wait(timeout=5)
            result=helper.stop_windows_owned(process,0,5)
            self.assertFalse(result['tree_confirmed'],result)
            self.assertEqual(result['final_total_processes'],2)
            self.assertEqual(result['observed_processes'],1)

    def test_observer_error_never_disappears_after_successful_final_snapshot(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);process=self.spawn(root);job=process._runner_job
            with patch.object(job,'_capture_for_tracking',side_effect=OSError('native observer failure')):
                wait_for(lambda:job.retained_members()[1] is not None)
            result=helper.stop_windows_owned(process,0,5)
            self.assertFalse(result['tree_confirmed'],result)
            self.assertIn('native observer failure',result['capture_error'])
            self.assertIsNotNone(process.poll())

    def test_identity_cap_fails_closed_without_growing_registry(self):
        with tempfile.TemporaryDirectory() as temp,patch.object(helper,'MAX_TRACKED_PROCESSES',1):
            root=Path(temp);process=self.spawn(root);job=process._runner_job
            (root/'spawn').touch();wait_for(lambda:child_pid(root))
            wait_for(lambda:job.retained_members()[1] is not None)
            self.assertEqual(len(job.retained_members()[0]),1)
            # The child is still owned by the Job even though observation was
            # capped; Job termination remains the only termination authority.
            result=helper.stop_windows_owned(process,0,5)
            self.assertFalse(result['tree_confirmed'],result)
            self.assertIsNotNone(process.poll())
            self.assertIsNone(job.handle)


if __name__=='__main__':unittest.main(verbosity=2)

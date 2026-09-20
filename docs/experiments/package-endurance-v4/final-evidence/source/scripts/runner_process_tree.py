"""Windows ownership for the campaign runner, independent of parent lifetime.

The direct child starts suspended, enters a non-breakaway kill-on-close Job,
and only then resumes. Cleanup observes the Job's ActiveProcesses counter;
waiting for only the direct child never confirms cleanup of its descendants.
"""
from __future__ import annotations

import ctypes
import os
import signal
import subprocess
import threading
import time
from ctypes import wintypes

MAX_TRACKED_PROCESSES = 4096
MAX_SNAPSHOT_ATTEMPTS = 3


class JobSnapshotChanged(OSError):
    """Job membership changed during a non-atomic accounting/PID snapshot."""


class WindowsLaunchCleanupError(OSError):
    """A failed launch still owns resources whose cleanup was not confirmed."""

    def __init__(self, launch_error, process, cleanup):
        super().__init__(f"{launch_error}; launch cleanup unconfirmed: {cleanup['errors']}")
        self.launch_error = launch_error
        self.process = process
        self.cleanup = cleanup


class _BasicLimits(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_longlong),
        ("PerJobUserTimeLimit", ctypes.c_longlong),
        ("LimitFlags", wintypes.DWORD),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _IoCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_ulonglong) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
    )]


class _ExtendedLimits(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _BasicLimits), ("IoInfo", _IoCounters),
        ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class _Accounting(ctypes.Structure):
    _fields_ = [(name, ctypes.c_longlong) for name in (
        "TotalUserTime", "TotalKernelTime", "ThisPeriodTotalUserTime", "ThisPeriodTotalKernelTime",
    )] + [(name, wintypes.DWORD) for name in (
        "TotalPageFaultCount", "TotalProcesses", "ActiveProcesses", "TotalTerminatedProcesses",
    )]


class _ThreadEntry(ctypes.Structure):
    _fields_ = [(name, wintypes.DWORD) for name in (
        "dwSize", "cntUsage", "th32ThreadID", "th32OwnerProcessID",
    )] + [("tpBasePri", wintypes.LONG), ("tpDeltaPri", wintypes.LONG), ("dwFlags", wintypes.DWORD)]


class WindowsJob:
    def __init__(self):
        if os.name != "nt":
            raise OSError("Windows Job Objects are unavailable on this platform")
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        self._observer_stop = threading.Event()
        self._observer = None
        self._observation_lock = threading.Lock()
        self._identities = {}
        self._observation_error = None
        signatures = {
            "CreateJobObjectW": ([ctypes.c_void_p, wintypes.LPCWSTR], wintypes.HANDLE),
            "SetInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD], wintypes.BOOL),
            "QueryInformationJobObject": ([wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p], wintypes.BOOL),
            "AssignProcessToJobObject": ([wintypes.HANDLE, wintypes.HANDLE], wintypes.BOOL),
            "TerminateJobObject": ([wintypes.HANDLE, wintypes.UINT], wintypes.BOOL),
            "CloseHandle": ([wintypes.HANDLE], wintypes.BOOL),
            "CreateToolhelp32Snapshot": ([wintypes.DWORD, wintypes.DWORD], wintypes.HANDLE),
            "Thread32First": ([wintypes.HANDLE, ctypes.POINTER(_ThreadEntry)], wintypes.BOOL),
            "Thread32Next": ([wintypes.HANDLE, ctypes.POINTER(_ThreadEntry)], wintypes.BOOL),
            "OpenThread": ([wintypes.DWORD, wintypes.BOOL, wintypes.DWORD], wintypes.HANDLE),
            "ResumeThread": ([wintypes.HANDLE], wintypes.DWORD),
            "OpenProcess": ([wintypes.DWORD, wintypes.BOOL, wintypes.DWORD], wintypes.HANDLE),
            "IsProcessInJob": ([wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.BOOL)], wintypes.BOOL),
            "WaitForSingleObject": ([wintypes.HANDLE, wintypes.DWORD], wintypes.DWORD),
            "GetProcessId": ([wintypes.HANDLE], wintypes.DWORD),
            "GetProcessTimes": ([wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)], wintypes.BOOL),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.api, name)
            function.argtypes, function.restype = args, result
        self.handle = self.api.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            limits = _ExtendedLimits()
            limits.BasicLimitInformation.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            self._check(self.api.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)))
        except BaseException:
            self.close()
            raise

    @staticmethod
    def _check(value):
        if not value:
            raise ctypes.WinError(ctypes.get_last_error())

    def assign(self, process):
        self._check(self.api.AssignProcessToJobObject(self.handle, int(process._handle)))
        # Enrollment happens while CREATE_SUSPENDED still prevents user code.
        # Every retained handle binds a native PID + creation-time identity.
        self._capture_for_tracking(time.monotonic() + 5)
        self._observer = threading.Thread(target=self._observe_members, daemon=True)
        self._observer.start()

    def _capture_for_tracking(self, deadline):
        for attempt in range(MAX_SNAPSHOT_ATTEMPTS):
            try:
                handles, total = self.capture_member_handles(deadline)
                break
            except JobSnapshotChanged:
                # Restart accounting and PID enumeration under the original
                # deadline. A retry never grants a fresh time budget.
                if attempt + 1 == MAX_SNAPSHOT_ATTEMPTS or time.monotonic() >= deadline:
                    raise
        try:
            for index, handle in enumerate(handles):
                created, exited, kernel, user = (wintypes.FILETIME() for _ in range(4))
                self._check(self.api.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited),
                                                    ctypes.byref(kernel), ctypes.byref(user)))
                pid = self.api.GetProcessId(handle)
                self._check(pid)
                identity = (pid, (created.dwHighDateTime << 32) | created.dwLowDateTime)
                with self._observation_lock:
                    if identity not in self._identities:
                        if len(self._identities) >= MAX_TRACKED_PROCESSES:
                            raise OSError("lifetime Job identity cap exceeded")
                        self._identities[identity] = handle
                        handles[index] = None  # ownership moved into the registry
            return total
        finally:
            errors = []
            for handle in handles:
                if handle and not self.api.CloseHandle(handle):
                    errors.append(ctypes.WinError(ctypes.get_last_error()))
            if errors:
                raise errors[0]

    def _observe_members(self):
        while not self._observer_stop.wait(.005):
            try:
                self._capture_for_tracking(time.monotonic() + .5)
            except JobSnapshotChanged:
                # Churn can exhaust this bounded pass without losing native
                # authority. Keep observing; final lifetime identity coverage
                # still rejects any historical member we never captured.
                continue
            except Exception as error:
                # Native lookup, identity, resource and observation failures
                # remain uncertainty even if a later snapshot is complete.
                with self._observation_lock:
                    self._observation_error = str(error)
                return

    def stop_observer(self, deadline):
        self._observer_stop.set()
        if self._observer is not None:
            self._observer.join(timeout=max(0, deadline-time.monotonic()))
            if self._observer.is_alive():
                raise OSError("Job lifetime observer did not join before cleanup deadline")

    def retained_members(self):
        with self._observation_lock:
            return list(self._identities.values()), self._observation_error

    def resume(self, process):
        # subprocess closes CreateProcess's primary-thread handle. Recover that
        # one thread while the process is still suspended, using documented APIs.
        snapshot = self.api.CreateToolhelp32Snapshot(0x00000004, 0)  # TH32CS_SNAPTHREAD
        if snapshot == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            entry = _ThreadEntry()
            entry.dwSize = ctypes.sizeof(entry)
            found = []
            present = self.api.Thread32First(snapshot, ctypes.byref(entry))
            while present:
                if entry.th32OwnerProcessID == process.pid:
                    found.append(entry.th32ThreadID)
                present = self.api.Thread32Next(snapshot, ctypes.byref(entry))
            if ctypes.get_last_error() != 18:  # ERROR_NO_MORE_FILES
                raise ctypes.WinError(ctypes.get_last_error())
            if len(found) != 1:
                raise OSError(f"expected one suspended primary thread, observed {len(found)}")
            thread = self.api.OpenThread(0x0002, False, found[0])  # THREAD_SUSPEND_RESUME
            if not thread:
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                if self.api.ResumeThread(thread) != 1:
                    raise OSError("initial thread did not have the expected suspend count")
            finally:
                self.api.CloseHandle(thread)
        finally:
            self.api.CloseHandle(snapshot)

    def active_processes(self):
        return self.accounting().ActiveProcesses

    def accounting(self):
        accounting = _Accounting()
        self._check(self.api.QueryInformationJobObject(
            self.handle, 1, ctypes.byref(accounting), ctypes.sizeof(accounting), None,
        ))
        return accounting

    def capture_member_handles(self, deadline):
        """Snapshot active members before signalling, retaining verified identities.

        A changing/truncated snapshot is evidence of incomplete capture, never
        an empty Job. The caller still terminates the owned Job on failure.
        """
        before = self.accounting()
        capacity = max(1, before.ActiveProcesses)
        while True:
            if capacity > MAX_TRACKED_PROCESSES or time.monotonic() >= deadline:
                raise OSError("Job member capture exceeded its bounded size/deadline")
            class ProcessIds(ctypes.Structure):
                _fields_ = [("assigned", wintypes.DWORD), ("count", wintypes.DWORD),
                            ("ids", ctypes.c_size_t * capacity)]
            ids = ProcessIds()
            ok = self.api.QueryInformationJobObject(self.handle, 3, ctypes.byref(ids), ctypes.sizeof(ids), None)
            if ok and ids.count == ids.assigned:
                break
            if not ok and ctypes.get_last_error() != 234:  # ERROR_MORE_DATA
                raise ctypes.WinError(ctypes.get_last_error())
            capacity = max(capacity * 2, ids.assigned)
        if ids.count != before.ActiveProcesses:
            raise JobSnapshotChanged("Job membership changed before handles could be captured")
        handles = []
        try:
            for pid in ids.ids[:ids.count]:
                if time.monotonic() >= deadline:
                    raise OSError("Job member handle capture deadline exhausted")
                handle = self.api.OpenProcess(0x00100000 | 0x1000, False, pid)  # wait + query only
                if not handle:
                    raise ctypes.WinError(ctypes.get_last_error())
                handles.append(handle)
                belongs = wintypes.BOOL()
                self._check(self.api.IsProcessInJob(handle, self.handle, ctypes.byref(belongs)))
                if not belongs.value:
                    raise OSError("a Job PID no longer identifies a member of this Job")
            after = self.accounting()
            if after.TotalProcesses != before.TotalProcesses:
                raise JobSnapshotChanged("new processes joined while Job handles were captured")
            return handles, before.TotalProcesses
        except BaseException:
            errors = []
            for handle in handles:
                if not self.api.CloseHandle(handle):
                    errors.append(ctypes.WinError(ctypes.get_last_error()))
            if errors:
                # A native close failure must not be hidden as retryable churn.
                raise errors[0]
            raise

    def wait_member(self, handle, deadline):
        remaining_ms = max(0, min(0xfffffffe, int((deadline-time.monotonic()) * 1000)))
        outcome = self.api.WaitForSingleObject(handle, remaining_ms)
        if outcome == 0:
            return True
        if outcome == 258:
            return False
        raise ctypes.WinError(ctypes.get_last_error())

    def terminate(self):
        self._check(self.api.TerminateJobObject(self.handle, 1))

    def close(self, deadline=None):
        deadline = time.monotonic()+5 if deadline is None else deadline
        try:
            self.stop_observer(deadline)
        except OSError:
            # Do not close handles under an unjoined native query. The Job
            # is still terminated and ownership remains for a later retry.
            if self.handle:
                self.terminate()
            raise
        errors = []
        with self._observation_lock:
            members = list(self._identities.items())
        for identity, member in members:
            if self.api.CloseHandle(member):
                with self._observation_lock:
                    del self._identities[identity]
            else:
                errors.append(ctypes.WinError(ctypes.get_last_error()))
        if self.handle:
            if self.api.CloseHandle(self.handle):
                self.handle = None
            else:
                errors.append(ctypes.WinError(ctypes.get_last_error()))
        if errors:
            raise errors[0]


def spawn_windows_owned(command, **kwargs):
    """Return a normal Popen retaining its Job through runner finalization."""
    job = WindowsJob()
    process = None
    try:
        process = subprocess.Popen(command, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | 0x00000004,
                                   **kwargs)  # CREATE_SUSPENDED
        process._runner_job = job
        job.assign(process)
        job.resume(process)
        return process
    except BaseException as launch_error:
        # Assignment failure never makes the child runnable. Resume failure
        # after assignment is covered by kill-on-close as well.
        errors = {}
        if process is not None:
            process._runner_launch_failed = True
            try:
                if process.poll() is None:
                    process.kill()
            except BaseException as error:
                errors["kill"] = repr(error)
            try:
                process.wait(timeout=10)
            except BaseException as error:
                errors["wait"] = repr(error)
            for name in ("stdin", "stdout", "stderr"):
                stream = getattr(process, name)
                if stream is not None:
                    try:
                        stream.close()
                    except BaseException as error:
                        errors[name] = repr(error)
        try:
            job.close()
        except BaseException as error:
            errors["job_close"] = repr(error)
        if errors:
            cleanup = {"outcome": "unconfirmed", "tree_confirmed": False,
                       "scope": "windows_launch", "errors": errors}
            raise WindowsLaunchCleanupError(launch_error, process, cleanup) from launch_error
        raise


def stop_windows_owned(process, graceful_timeout_s, kill_timeout_s):
    job = getattr(process, "_runner_job", None)
    result = {"graceful_signal": None, "taskkill_exit": None, "outcome": "unconfirmed",
              "scope": "windows_job", "tree_confirmed": False, "active_processes": None}
    end = time.monotonic() + max(graceful_timeout_s, 0) + max(kill_timeout_s, 0)
    if job is None or job.handle is None:
        # An injected/unowned Popen cannot prove that its descendants exited.
        result.update(scope="unowned_process", detail="no retained Job; process tree cannot be verified")
        try:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=max(kill_timeout_s, 0))
        finally:
            # A previous Job close may have succeeded while an observation
            # handle close failed; those retained handles still need a retry.
            if job is not None:
                job.close(end)
        return result
    if getattr(process, "_runner_launch_failed", False) and process.poll() is None:
        # The suspended direct child may never have entered the Job. Its Popen
        # handle remains our authority; retry that handle, never a PID lookup.
        try:
            process.kill()
        except OSError as error:
            result["launch_kill_error"] = repr(error)
    parent_already_exited = process.poll() is not None
    members = []
    captured_total = None
    capture_error = None
    try:
        try:
            job.stop_observer(end)
            captured_total = job._capture_for_tracking(end)
            members, observation_error = job.retained_members()
            if observation_error:
                raise OSError("lifetime observer failed: " + observation_error)
            result["retained_member_handles"] = len(members)
            result["captured_total_processes"] = captured_total
        except OSError as error:
            capture_error = str(error)
            result["capture_error"] = capture_error
            members, _ = job.retained_members()
        if not parent_already_exited and capture_error is None:
            try:
                process.send_signal(signal.CTRL_BREAK_EVENT)
                result["graceful_signal"] = "CTRL_BREAK_EVENT"
            except (OSError, ValueError):
                pass
            try:
                process.wait(timeout=min(max(graceful_timeout_s, 0), max(0, end-time.monotonic())))
            except subprocess.TimeoutExpired:
                pass
        active = job.active_processes()
        result["active_before_kill"] = active
        if active:
            job.terminate()
        while active:
            active = job.active_processes()
            if not active or time.monotonic() >= end:
                break
            time.sleep(min(.02, max(0, end - time.monotonic())))
        result["active_processes"] = active
        if not active:
            result["final_total_processes"] = job.accounting().TotalProcesses
            stable_capture = captured_total is not None and result["final_total_processes"] == captured_total
            observed_processes = len(members)
            result["observed_processes"] = observed_processes
            complete_identity_coverage = captured_total == observed_processes
            if not stable_capture:
                result["detail"] = "Job member capture was incomplete or new processes joined before termination"
            elif not complete_identity_coverage:
                result["detail"] = "historical Job members lack retained exit-observation handles"
            members_exited = all(job.wait_member(handle, end) for handle in members)
            result["retained_members_exited"] = members_exited
            # Job accounting can reach zero just before the direct process's
            # retained HANDLE becomes signaled. A nonblocking poll in that
            # window is not proof of failed cleanup: use the remaining same
            # deadline to wait for the independent process-exit observation.
            try:
                process.wait(timeout=max(0, end - time.monotonic()))
            except subprocess.TimeoutExpired:
                result["detail"] = "direct process exit was not observed before the cleanup deadline"
                return result
            if stable_capture and complete_identity_coverage and members_exited and capture_error is None:
                result["tree_confirmed"] = True
                result["outcome"] = "already_exited" if parent_already_exited and not result["active_before_kill"] else "terminated_confirmed"
        return result
    finally:
        job.close(end)

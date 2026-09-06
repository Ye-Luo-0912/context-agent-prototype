#!/usr/bin/env python3
"""Linux process-lifetime mechanism probe, NOT a Rust/Agent integration test.
A temporary supervisor is SIGKILLed; its separately grouped silent child survives.
This process is made a subreaper so it can kill AND wait for the orphan itself.
"""
from __future__ import annotations
import ctypes
import json
import os
import selectors
import signal
import subprocess
import sys
import time


def main() -> None:
    if not sys.platform.startswith("linux"):
        raise SystemExit("Linux /proc and PR_SET_CHILD_SUBREAPER are required.")
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
        raise OSError(ctypes.get_errno(), "cannot become probe-local subreaper")
    parent_code = r'''
import json, os, signal, subprocess, sys
signal.alarm(6)
c = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.alarm(5); time.sleep(20)"],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL, process_group=0)
print(json.dumps({"child_pid":c.pid,"child_pgid":os.getpgid(c.pid)}), flush=True)
c.wait()
'''
    parent = subprocess.Popen([sys.executable, "-u", "-c", parent_code],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, start_new_session=True)
    child_pid = None
    child_reaped = False
    child_status = None
    try:
        assert parent.stdout is not None
        with selectors.DefaultSelector() as sel:
            sel.register(parent.stdout, selectors.EVENT_READ)
            if not sel.select(2):
                raise RuntimeError("temporary supervisor did not report its child")
            identities = json.loads(parent.stdout.readline())
        child_pid = int(identities["child_pid"])
        assert int(identities["child_pgid"]) == child_pid
        parent.kill()  # SIGKILL only the supervisor, not its child's process group.
        _, parent_stderr = parent.communicate(timeout=2)
        status_text = open(f"/proc/{child_pid}/status", encoding="utf-8").read()
        state_line = next(line for line in status_text.splitlines() if line.startswith("State:"))
        ppid_line = next(line for line in status_text.splitlines() if line.startswith("PPid:"))
        live = "Z" not in state_line and "X" not in state_line
    finally:
        if parent.poll() is None:
            parent.kill()
            parent.communicate(timeout=2)
        if child_pid is not None:
            try:
                os.killpg(child_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                try:
                    reaped_pid, status = os.waitpid(child_pid, os.WNOHANG)
                except ChildProcessError:
                    break
                if reaped_pid == child_pid:
                    child_reaped, child_status = True, status
                    break
                time.sleep(0.005)
    assert child_reaped, "Probe cleanup could not confirm waitpid; do not report success."
    assert parent.returncode == -signal.SIGKILL and live
    print(json.dumps({
        "test_kind":"OS_MECHANISM_ONLY_NOT_REPOSITORY_TEST",
        "source_sha":"12c86283b8d5991e9f17a07f14871dcf39d65066",
        "source_path":"crates/tool-runtime/src/tools/process.rs",
        "simulated_spawn_attribute":"process_group(0), no parent-death containment",
        "supervisor_exit_code":parent.returncode,
        "silent_child_survived_supervisor_sigkill":live,
        "child_state_after_supervisor_death":state_line,
        "child_reparented_to_probe_subreaper":int(ppid_line.split(':',1)[1]) == os.getpid(),
        "supervisor_reaped":parent.poll() is not None,
        "child_killed_and_reaped":child_reaped,
        "child_wait_status":child_status,
        "parent_stderr":parent_stderr,
        "repository_binary_executed":False,
        "rust_tests_executed":False
    },ensure_ascii=False,indent=2))

if __name__ == "__main__":
    main()

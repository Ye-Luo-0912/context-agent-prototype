#!/usr/bin/env python3
"""Linux/POSIX mechanism probe, NOT a test of the repository's Rust binary.
Uses the flags observed in ConfinedDir::open_existing and compares O_NONBLOCK.
All children are watchdog-bounded, killed when needed, and reaped.
"""
from __future__ import annotations
import json
import os
import pathlib
import selectors
import signal
import stat
import subprocess
import sys
import tempfile
import time


def main() -> None:
    if os.name != "posix" or not hasattr(os, "mkfifo"):
        raise SystemExit("This mechanism probe requires POSIX mkfifo.")
    child_code = r'''
import os, signal, sys
signal.alarm(3)
d = os.open(sys.argv[1], os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
print("entering_openat", flush=True)
f = os.open("Cargo.toml", os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=d)
print("open_returned", flush=True)
os.close(f)
os.close(d)
'''
    with tempfile.TemporaryDirectory(prefix="audit-fifo-") as root:
        fifo = pathlib.Path(root) / "Cargo.toml"
        os.mkfifo(fifo, 0o600)
        child = subprocess.Popen([sys.executable, "-u", "-c", child_code, root],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, start_new_session=True)
        try:
            assert child.stdout is not None
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                if not selector.select(1.5):
                    raise RuntimeError("child failed to reach open probe within watchdog")
                ready = child.stdout.readline().strip()
            start = time.monotonic()
            try:
                stdout, stderr = child.communicate(timeout=0.3)
                blocked = False
            except subprocess.TimeoutExpired:
                blocked = child.poll() is None
                os.killpg(child.pid, signal.SIGKILL)
                stdout, stderr = child.communicate(timeout=1.5)
            observed_ms = (time.monotonic() - start) * 1000
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGKILL)
                child.communicate(timeout=1.5)
        d = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            start = time.monotonic()
            fd = os.open("Cargo.toml", os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=d)
            try:
                mode = os.fstat(fd).st_mode
                nonblocking_ms = (time.monotonic() - start) * 1000
            finally:
                os.close(fd)
        finally:
            os.close(d)
        result = {
            "test_kind": "OS_MECHANISM_ONLY_NOT_REPOSITORY_TEST",
            "source_sha": "12c86283b8d5991e9f17a07f14871dcf39d65066",
            "source_path": "crates/agent-workspace/src/confined.rs",
            "source_function": "ConfinedDir::open_existing (Unix)",
            "named_entry": "Cargo.toml (FIFO, no writer)",
            "child_ready_marker": ready,
            "observed_blocked_without_nonblock": blocked,
            "watchdog_observation_ms": round(observed_ms, 3),
            "child_reaped": child.poll() is not None,
            "child_returncode": child.returncode,
            "stdout_after_ready": stdout,
            "stderr": stderr,
            "nonblocking_open_ms": round(nonblocking_ms, 3),
            "same_handle_is_fifo": stat.S_ISFIFO(mode),
            "same_handle_is_regular": stat.S_ISREG(mode),
            "repository_binary_executed": False,
            "rust_tests_executed": False,
        }
    print(json.dumps(result, ensure_ascii=False, indent=2))

if __name__ == "__main__":
    main()

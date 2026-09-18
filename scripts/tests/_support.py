"""Shared local fixtures for the runtime endurance script tests.

Everything here is stubbed: a local HTTP upstream, stub child processes and
temporary campaign directories. No eval.env is read and no real provider is
contacted.
"""
from __future__ import annotations

import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
REPO = SCRIPTS.parent

sys.path.insert(0, str(SCRIPTS))

import runtime_endurance_incremental_runner as runner_mod  # noqa: E402
import runtime_endurance_full_campaign as campaign_mod  # noqa: E402

runner = runner_mod
campaign = campaign_mod

PROTECTED_FILES = (
    "TASK.md",
    "tests/test_platform.py",
    "fixtures/input.jsonl",
    "fixtures/plan.json",
    "oracle.py",
)

HEAD = "0123456789abcdef0123456789abcdef01234567"

USAGE_FULL = {"input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 100}


def make_campaign(root: Path) -> Path:
    """Create a minimal campaign directory with the protected seed files."""
    workspace = root / "workspace"
    for name in PROTECTED_FILES:
        path = workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(f"seed {name}\n", encoding="utf-8")
    return root


def minimal_env(upstream_base: str | None = None) -> dict[str, str]:
    keys = ("SystemRoot", "SYSTEMDRIVE", "COMSPEC", "PATHEXT", "TEMP", "TMP", "PROCESSOR_ARCHITECTURE", "APPDATA")
    env = {key: os.environ[key] for key in keys if key in os.environ}
    env["OPENAI_MODEL"] = "deepseek-flash"
    if upstream_base:
        env["OPENAI_BASE_URL"] = upstream_base
    return env


def base_config(campaign: Path, **overrides):
    config = runner_mod.RunnerConfig(
        segment="seg-001",
        campaign_dir=campaign,
        work_dir=campaign / "workspace",
        repo=REPO,
        binary=Path(sys.executable),
        mode="work",
        prompt_text="stub prompt for local tests",
        rounds=2,
        max_cost_usd=1.5,
        max_output_tokens=8192,
        child_wait_timeout_s=20.0,
        child_graceful_timeout_s=1.0,
        child_kill_timeout_s=8.0,
        upstream_timeout_s=5.0,
        env=minimal_env("http://127.0.0.1:9"),
        head=HEAD,
        identity_head=HEAD,
        capture_git_diff=False,
    )
    for key, value in overrides.items():
        setattr(config, key, value)
    return config


def pid_alive(pid: int) -> bool:
    """True only if the process behind pid is still running."""
    if os.name == "nt":
        import ctypes

        SYNCHRONIZE = 0x00100000
        WAIT_TIMEOUT = 0x00000102
        kernel32 = ctypes.windll.kernel32
        handle = kernel32.OpenProcess(SYNCHRONIZE, False, int(pid))
        if not handle:
            return False
        try:
            return kernel32.WaitForSingleObject(handle, 0) == WAIT_TIMEOUT
        finally:
            kernel32.CloseHandle(handle)
    try:
        os.kill(int(pid), 0)
        return True
    except OSError:
        return False


# --- stub child process sources -------------------------------------------------

CHILD_EXIT_0 = "import sys\nsys.exit(0)\n"
CHILD_EXIT_7 = "import sys\nsys.exit(7)\n"
CHILD_SLEEPER = "import time\ntime.sleep(60)\n"
CHILD_TAMPER = (
    "import sys\n"
    "with open(sys.argv[1], 'a', encoding='utf-8') as handle:\n"
    "    handle.write('tampered-by-stub\\n')\n"
)
CHILD_REQUEST = (
    "import json, os, sys, threading, time, urllib.request\n"
    "base = os.environ['OPENAI_BASE_URL']\n"
    "count = int(sys.argv[1])\n"
    "gap = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0\n"
    "body = json.dumps({'model': 'stub', 'stream': True, 'input': 'hello world'}).encode()\n"
    "def one():\n"
    "    request = urllib.request.Request(base + '/responses', data=body,"
    " headers={'Content-Type': 'application/json'}, method='POST')\n"
    "    try:\n"
    "        with urllib.request.urlopen(request, timeout=60) as response:\n"
    "            response.read()\n"
    "    except Exception as error:\n"
    "        sys.stderr.write('request failed: %r\\n' % (error,))\n"
    "first = threading.Thread(target=one)\n"
    "first.start()\n"
    "if count > 1:\n"
    "    time.sleep(gap)\n"
    "    one()\n"
    "first.join()\n"
    "sys.exit(0)\n"
)

REQUEST_BODY = json.dumps({"model": "stub", "stream": True, "input": "hello world"}).encode()


def stub_file(root: Path, name: str, source: str) -> str:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(source, encoding="utf-8")
    return str(path)


def read_json(path) -> dict:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def _sse(usage: dict) -> bytes:
    return b'data: {"response": {"usage": ' + json.dumps(usage).encode() + b'}}\n\ndata: [DONE]\n\n'


class StubUpstream:
    """Local HTTP stub standing in for the model provider.

    Modes:
      ok            complete SSE stream carrying full usage
      missing_cached usage block missing cached_input_tokens
      rate_limited  HTTP 429
      stall         200 + one comment chunk, then silence for stall_s seconds
      delay         sleep delay_s, then a complete "ok" stream
    """

    def __init__(self, mode: str = "ok", stall_s: float = 30.0, delay_s: float = 0.0):
        outer = self
        self.mode = mode
        self.requests: list[int] = []
        stall = stall_s
        delay = delay_s

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                outer.requests.append(len(outer.requests) + 1)
                if outer.mode == "rate_limited":
                    body = b'{"error": {"message": "rate limited"}}'
                    self.send_response(429)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                if outer.mode == "stall":
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.end_headers()
                    self.wfile.write(b": ping\n\n")
                    self.wfile.flush()
                    time.sleep(stall)
                    return
                if delay:
                    time.sleep(delay)
                usage = dict(USAGE_FULL)
                if outer.mode == "missing_cached":
                    usage.pop("cached_input_tokens")
                body = _sse(usage)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()

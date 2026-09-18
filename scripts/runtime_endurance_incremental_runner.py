"""Bounded real-model runner for the incremental-platform campaign.

Runs one agent-tui child process per segment behind a local forwarding relay,
with a campaign-persistent budget ledger (reserve-before-accept), an
unconditional child/relay finalization and layered machine-readable exit
codes.

Exit-code semantics (outer orchestration only needs this process's exit code;
every nonzero run also records the detail in receipts under `categories`):

  0   success: child exited 0, protected files unchanged, no recovery fence,
      budget fully settled (no unknown attempt, intake never cap-stopped),
      cleanup confirmed.
  2   invalid configuration (non-finite or negative --max-cost-usd /
      --max-output-tokens / --rounds, or missing provider environment).
  10  child exited nonzero. The child's own exit code (including its 0..4
      semantics) is preserved verbatim as `child_exit_code` in receipts and is
      NOT remapped.
  11  protected fixture files changed during the segment
      (protected_unchanged=false).
  12  recovery fence: events contain a recovery_required terminal.
  13  budget incomplete: at least one attempt settled as unknown usage, intake
      stopped by the cap (cap_stopped), or the persistent budget ledger is
      unreadable. Missing usage is never guessed at zero.
  14  cleanup unconfirmed: the child process tree could not be confirmed dead
      during finalization.
  15  child launch failed (missing or unspawnable binary).
  16  campaign identity mismatch (head/binary hash differs from the recorded
      baseline); no paid request is accepted in that case.
  17  interrupted (KeyboardInterrupt); the same unconditional finalization
      ran and the receipt outcome is "interrupted".
  18  segment directory already exists; refused without touching it.
  19  child wait timed out; the runner stopped the child itself (the child
      code recorded afterwards belongs to the runner's stop, not to the task).
  20  unexpected internal error; terminal receipts are still written.

When several categories apply, the exit code is the first match in the order
above (2 < 16/18 < 17 < 14 < 19 < 10 < 11 < 12 < 13 < 20 by severity of
"refuse to run" over "report outcome"); the full category list is always in
the receipt.

The child's stdout/stderr logs, pid.txt, events.jsonl, per-request captures
and all receipts live under the segment directory and are preserved on every
path, including timeouts, interrupts and crashes. Normal, exceptional and
interrupted runs each write metadata.json, summary.json and usage-ledger.json
before the process exits.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import io
import json
import math
import os
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CAMPAIGN = REPO / "target" / "runtime-endurance-v1" / "incremental-platform-20260919"
WORK = CAMPAIGN / "workspace"

WINDOWS = sys.platform == "win32"

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_CHILD_FAILED = 10
EXIT_PROTECTED_MODIFIED = 11
EXIT_RECOVERY_REQUIRED = 12
EXIT_BUDGET_INCOMPLETE = 13
EXIT_CLEANUP_UNCONFIRMED = 14
EXIT_LAUNCH_FAILED = 15
EXIT_IDENTITY_MISMATCH = 16
EXIT_INTERRUPTED = 17
EXIT_SEGMENT_EXISTS = 18
EXIT_CHILD_TIMEOUT = 19
EXIT_INTERNAL_ERROR = 20

_EXIT_PRIORITY = (
    ("invalid_config", EXIT_USAGE),
    ("identity_mismatch", EXIT_IDENTITY_MISMATCH),
    ("segment_exists", EXIT_SEGMENT_EXISTS),
    ("interrupted", EXIT_INTERRUPTED),
    ("cleanup_unconfirmed", EXIT_CLEANUP_UNCONFIRMED),
    ("child_wait_timeout", EXIT_CHILD_TIMEOUT),
    ("child_nonzero", EXIT_CHILD_FAILED),
    ("protected_modified", EXIT_PROTECTED_MODIFIED),
    ("recovery_required", EXIT_RECOVERY_REQUIRED),
    ("budget_incomplete", EXIT_BUDGET_INCOMPLETE),
    ("budget_ledger_unreadable", EXIT_BUDGET_INCOMPLETE),
    ("child_launch_failed", EXIT_LAUNCH_FAILED),
    ("internal_error", EXIT_INTERNAL_ERROR),
)


def exit_code_for(categories) -> int:
    for name, code in _EXIT_PRIORITY:
        if name in categories:
            return code
    return EXIT_OK


class ConfigError(ValueError):
    """Invalid runner configuration or provider environment."""


class ChildLaunchError(RuntimeError):
    """The child process could not be spawned."""


class _BudgetLedgerError(RuntimeError):
    """The persistent budget ledger exists but cannot be trusted."""


@dataclass(frozen=True)
class Pricing:
    """Wire pricing used for every estimated amount in receipts.

    All amounts derived here are ESTIMATES for budget enforcement, never real
    bills. The profile name is recorded alongside so a stale estimate cannot
    be mistaken for another profile's accounting.
    """

    name: str = "deepseek-flash-2026-09"
    input_per_mtoken_usd: float = 0.30
    cached_input_per_mtoken_usd: float = 0.006
    output_per_mtoken_usd: float = 1.20

    def attempt_cost_usd(self, input_tokens: int, cached_input_tokens: int, output_tokens: int) -> float:
        miss = max(input_tokens - cached_input_tokens, 0)
        return (
            miss * self.input_per_mtoken_usd
            + cached_input_tokens * self.cached_input_per_mtoken_usd
            + output_tokens * self.output_per_mtoken_usd
        ) / 1_000_000

    def reserve_estimate_usd(self, request_body_bytes: int, max_output_tokens: int) -> float:
        """Conservative upper bound reserved before a request is accepted.

        Strategy: assume the whole output budget (max_output_tokens) is
        produced at output price, and the prompt is request_body_bytes/4 + 256
        tokens, all priced as cache-miss input.
        """
        input_tokens_estimate = request_body_bytes / 4 + 256
        return (
            input_tokens_estimate * self.input_per_mtoken_usd
            + max_output_tokens * self.output_per_mtoken_usd
        ) / 1_000_000


# Usage parsing is tied to one wire protocol. These field names belong to the
# OpenAI Responses event schema; switching to another endpoint protocol
# requires adapting this constant explicitly. Never reuse it based on the
# model name alone.
RESPONSES_USAGE_SCHEMA = {
    "protocol": "openai-responses",
    "required_fields": ("input_tokens", "cached_input_tokens", "output_tokens"),
}


def parse_usage_strict(raw: bytes):
    """Parse the last usage block from a captured SSE stream.

    Returns (usage, None) with all required fields present as integers, or
    (None, reason) when no usage block was streamed or any required field is
    missing/non-integer. Missing data is never zero-filled.
    """
    latest = None
    for line in raw.splitlines():
        if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
            continue
        try:
            event = json.loads(line[6:])
        except Exception:
            continue
        usage = (event.get("response") or {}).get("usage") or event.get("usage")
        if isinstance(usage, dict):
            latest = usage
    if latest is None:
        return None, "usage_missing"
    parsed = {}
    for name in RESPONSES_USAGE_SCHEMA["required_fields"]:
        value = latest.get(name)
        if isinstance(value, bool) or not isinstance(value, int):
            return None, f"usage_incomplete:{name}"
        parsed[name] = value
    return parsed, None


def digest(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def _safe_digest(path: Path):
    try:
        return digest(path)
    except OSError:
        return None


def _write_json(path: Path, payload) -> None:
    """Atomic JSON write: temp file in the same directory + os.replace."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    with tmp.open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)


def _read_json_file(path: Path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def _git_head(repo: Path):
    try:
        return subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=str(repo), text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def _print_json(payload) -> None:
    text = json.dumps(payload, ensure_ascii=False)
    try:
        print(text)
    except UnicodeEncodeError:
        print(json.dumps(payload, ensure_ascii=True))


def _protected_hashes(work_dir: Path, names) -> dict[str, str]:
    return {name: _safe_digest(Path(work_dir) / name) for name in names}


def build_env(repo: Path) -> dict[str, str]:
    """Production environment: scrub ambient provider variables, load eval.env."""
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("OPENAI_", "MAINTENANCE_")) and k not in ("AGENT_AUTO_APPROVE", "AGENT_DEMO")
    }
    for line in (Path(repo) / "eval.env").read_text(encoding="utf-8-sig").splitlines():
        if "=" in line and not line.lstrip().startswith("#"):
            key, value = line.split("=", 1)
            env[key.strip()] = value.strip().strip('"').strip("'")
    if env.get("OPENAI_MODEL") != "deepseek-flash":
        raise ConfigError("campaign requires the configured deepseek-flash profile")
    return env


@dataclass
class RunnerConfig:
    """Injectable runner settings (paths, env, child command, timeouts)."""

    segment: str
    campaign_dir: Path = CAMPAIGN
    work_dir: Path | None = None
    repo: Path = REPO
    binary: Path = REPO / "target/debug/agent-tui.exe"
    child_command: list | None = None
    mode: str = "work"
    prompt_file: str | None = None
    prompt_text: str | None = None
    rounds: int = 24
    max_cost_usd: float = 1.5
    max_output_tokens: int = 8192
    child_wait_timeout_s: float = 780.0
    child_graceful_timeout_s: float = 30.0
    child_kill_timeout_s: float = 20.0
    upstream_timeout_s: float = 150.0
    env: dict | None = None
    pricing: Pricing = field(default_factory=Pricing)
    protected_files: tuple = (
        "TASK.md",
        "tests/test_platform.py",
        "fixtures/input.jsonl",
        "fixtures/plan.json",
        "oracle.py",
    )
    head: str | None = None
    identity_head: str | None = None
    capture_git_diff: bool = True
    interrupt_event: threading.Event | None = None
    ledger_path: Path | None = None
    upstream_url: str | None = None


def _validate_config(cfg: RunnerConfig) -> None:
    for name, value in (
        ("--max-cost-usd", cfg.max_cost_usd),
        ("--max-output-tokens", cfg.max_output_tokens),
        ("--rounds", cfg.rounds),
    ):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ConfigError(f"{name} must be a finite number, got {value!r}")
        if isinstance(value, float):
            if not math.isfinite(value):
                raise ConfigError(f"{name} must be finite, got {value!r}")
        elif name == "--rounds" and not isinstance(value, int):
            raise ConfigError(f"{name} must be an integer, got {value!r}")
        if value < 0:
            raise ConfigError(f"{name} must be non-negative, got {value!r}")


class BudgetLedger:
    """Campaign-persistent budget ledger (atomic JSON, cross-process).

    Structure:
      schema          1
      cap_usd         cap enforced by the most recent run
      committed_usd   settled attempts with known usage
      reserved_usd    open reservations (in-flight attempts)
      unknown_usd     attempts whose usage could not be determined; they keep
                      occupying budget exactly like committed money
      cap_stopped     true once a reservation was refused for exceeding the cap
      attempts[]      monotonic `id`, per-segment `request` number, `segment`,
                      settlement `status` (reserved / committed / unknown /
                      rejected_cap / upstream_rate_limited), reserved and
                      settled amounts, detail
    """

    SCHEMA = 1

    def __init__(self, path: Path, data: dict, pricing: Pricing, max_output_tokens: int, cap_usd: float):
        self.path = Path(path)
        self.data = data
        self.pricing = pricing
        self.max_output_tokens = max_output_tokens
        self.cap_usd = cap_usd
        self.lock = threading.RLock()
        data.setdefault("schema", self.SCHEMA)
        data.setdefault("committed_usd", 0.0)
        data.setdefault("reserved_usd", 0.0)
        data.setdefault("unknown_usd", 0.0)
        data.setdefault("cap_stopped", False)
        data.setdefault("attempts", [])
        self._next_id = max((attempt.get("id", 0) for attempt in data["attempts"]), default=0) + 1
        data["cap_usd"] = cap_usd
        data["pricing"] = asdict(pricing)
        data["reserve_policy"] = self._reserve_policy()

    def _reserve_policy(self) -> dict:
        return {
            "strategy": "conservative_upper_bound",
            "input_estimate": "request_body_bytes/4 + 256 tokens, priced as cache-miss input",
            "output_estimate": "max_output_tokens priced at output rate",
            "max_output_tokens": self.max_output_tokens,
            "estimated": True,
        }

    @classmethod
    def load(cls, path: Path, cap_usd: float, pricing: Pricing, max_output_tokens: int):
        """Load the campaign ledger or start a new one; returns (ledger, inherited)."""
        path = Path(path)
        inherited = path.exists()
        if inherited:
            data = _read_json_file(path)
            if not isinstance(data, dict) or not isinstance(data.get("attempts"), list):
                raise _BudgetLedgerError(f"existing budget ledger unreadable or unrecognized: {path}")
        else:
            data = {"origin": "new"}
        ledger = cls(path, data, pricing, max_output_tokens, cap_usd)
        ledger._save()
        return ledger, inherited

    def _save(self) -> None:
        _write_json(self.path, self.data)

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "cap_usd": self.data.get("cap_usd"),
                "committed_usd": self.data.get("committed_usd", 0.0),
                "reserved_usd": self.data.get("reserved_usd", 0.0),
                "unknown_usd": self.data.get("unknown_usd", 0.0),
                "cap_stopped": bool(self.data.get("cap_stopped")),
                "attempts": copy.deepcopy(self.data.get("attempts", [])),
                "reserve_policy": copy.deepcopy(self.data.get("reserve_policy", {})),
            }

    def reserve(self, request_number: int, body_len: int, segment: str):
        """Reserve a bounded estimate before the request is accepted.

        Returns (attempt_id, estimate, rejected). A rejected request is
        recorded with settled_usd=0 and never forwarded upstream.
        """
        with self.lock:
            estimate = self.pricing.reserve_estimate_usd(body_len, self.max_output_tokens)
            attempt_id = self._next_id
            self._next_id += 1
            occupied = self.data["committed_usd"] + self.data["reserved_usd"] + self.data["unknown_usd"]
            if self.data["cap_stopped"] or occupied + estimate > self.cap_usd:
                self.data["cap_stopped"] = True
                self.data["attempts"].append(
                    {
                        "id": attempt_id,
                        "request": request_number,
                        "segment": segment,
                        "status": "rejected_cap",
                        "reserved_usd": 0.0,
                        "settled_usd": 0.0,
                        "estimate_usd": estimate,
                        "detail": "reserve exceeds cap; request refused before forwarding",
                    }
                )
                self._save()
                return None, estimate, True
            self.data["reserved_usd"] += estimate
            self.data["attempts"].append(
                {
                    "id": attempt_id,
                    "request": request_number,
                    "segment": segment,
                    "status": "reserved",
                    "reserved_usd": estimate,
                    "settled_usd": 0.0,
                    "estimate_usd": estimate,
                    "detail": "",
                }
            )
            self._save()
            return attempt_id, estimate, False

    def settle(self, attempt_id, status: str, usage=None, detail: str = "", http_status=None) -> None:
        """Settle a reserved attempt exactly once; unknown keeps the reserve."""
        with self.lock:
            attempt = next((a for a in self.data["attempts"] if a.get("id") == attempt_id), None)
            if attempt is None or attempt["status"] != "reserved":
                return
            self.data["reserved_usd"] = max(self.data["reserved_usd"] - attempt["reserved_usd"], 0.0)
            attempt["detail"] = detail or attempt["detail"]
            attempt["http_status"] = http_status
            if status == "committed" and usage:
                cost = self.pricing.attempt_cost_usd(
                    usage["input_tokens"], usage["cached_input_tokens"], usage["output_tokens"]
                )
                self.data["committed_usd"] += cost
                attempt.update(
                    status="committed",
                    settled_usd=cost,
                    input_tokens=usage["input_tokens"],
                    cached_input_tokens=usage["cached_input_tokens"],
                    output_tokens=usage["output_tokens"],
                    estimated_usd=cost,
                )
            elif status == "upstream_rate_limited":
                attempt.update(status="upstream_rate_limited", settled_usd=0.0)
            else:
                self.data["unknown_usd"] += attempt["reserved_usd"]
                attempt.update(status="unknown", settled_usd=attempt["reserved_usd"])
            self._save()

    def force_settle_segment(self, segment: str, detail: str) -> list:
        """Settle every still-open reservation of a segment as unknown."""
        forced = []
        with self.lock:
            for attempt in list(self.data["attempts"]):
                if attempt.get("segment") == segment and attempt.get("status") == "reserved":
                    forced.append(attempt["id"])
                    self.settle(attempt["id"], "unknown", detail=detail)
        return forced


def _read_chunk(response):
    read1 = getattr(response, "read1", None)
    if read1 is None:
        return response.read(16384)
    try:
        return read1(16384)
    except io.UnsupportedOperation:
        return response.read(16384)


class _Relay:
    def __init__(self, upstream: str, ledger: BudgetLedger, out_dir: Path, segment: str, upstream_timeout_s: float):
        self.upstream = upstream
        self.ledger = ledger
        self.out_dir = Path(out_dir)
        self.segment = segment
        self.upstream_timeout_s = upstream_timeout_s
        self.lock = threading.Lock()
        self.wire: list = []
        self.closing = False
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _build_relay_handler(self))
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True, name="runner-relay")
        self.thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    def close(self) -> None:
        self.closing = True
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def _build_relay_handler(relay: _Relay):
    class RelayHandler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def _send_json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):  # noqa: N802
            if relay.closing:
                self._send_json(503, {"error": "relay is shutting down; no new requests accepted"})
                return
            try:
                length = int(self.headers.get("Content-Length") or 0)
            except ValueError:
                length = 0
            body = self.rfile.read(length)
            with relay.lock:
                number = len(relay.wire) + 1
                relay.wire.append({"request": number, "path": self.path})
                # reserve-before-accept: numbering and the budget check share
                # this one lock so concurrent requests cannot oversubscribe.
                attempt_id, _estimate, rejected = relay.ledger.reserve(number, len(body), relay.segment)
            if rejected:
                # refused before any upstream contact: nothing to bill
                self._send_json(429, {"error": "bounded campaign cost cap reached"})
                return
            (relay.out_dir / f"request-{number:03d}.json").write_bytes(body)
            status_word = "unknown"
            detail = ""
            usage = None
            upstream_status = None
            headers_sent = False
            try:
                request = urllib.request.Request(
                    relay.upstream + "/" + self.path.removeprefix("/v1/"),
                    data=body,
                    headers={
                        "Content-Type": "application/json",
                        "Authorization": self.headers.get("Authorization", ""),
                        "Accept": "text/event-stream",
                    },
                    method="POST",
                )
                try:
                    response = urllib.request.urlopen(request, timeout=relay.upstream_timeout_s)
                except urllib.error.HTTPError as error:
                    response = error
                upstream_status = getattr(response, "status", None) or getattr(response, "code", 0)
                response_bytes = bytearray()
                self.send_response(upstream_status)
                self.send_header("Content-Type", response.headers.get("Content-Type", "text/event-stream"))
                self.end_headers()
                headers_sent = True
                with response, (relay.out_dir / f"response-{number:03d}.sse").open("wb") as captured:
                    while True:
                        chunk = _read_chunk(response)
                        if not chunk:
                            break
                        captured.write(chunk)
                        if len(response_bytes) <= 2 * 1024 * 1024:
                            response_bytes.extend(chunk)
                        try:
                            self.wfile.write(chunk)
                            self.wfile.flush()
                        except (BrokenPipeError, ConnectionResetError):
                            break
                if upstream_status == 429:
                    status_word = "upstream_rate_limited"
                    detail = "upstream returned 429; not billed"
                elif upstream_status >= 400:
                    status_word = "unknown"
                    detail = f"http_error_{upstream_status}_without_usage"
                else:
                    usage, problem = parse_usage_strict(bytes(response_bytes))
                    if usage is None:
                        status_word = "unknown"
                        detail = problem or "usage_missing"
                    else:
                        status_word = "committed"
            except Exception as error:
                status_word = "unknown"
                detail = f"upstream_failure:{type(error).__name__}"
                if not headers_sent:
                    try:
                        self.send_error(502, "upstream transport failed")
                    except Exception:
                        pass
            finally:
                # guaranteed settlement on every path, including open failures
                # and interrupted streams
                relay.ledger.settle(attempt_id, status_word, usage=usage, detail=detail, http_status=upstream_status)

    return RelayHandler


def _verify_campaign_identity(cfg: RunnerConfig, campaign_dir: Path):
    """Compare current head/binary against recorded campaign identity.

    Returns None when identity matches (writing identity.json on first use) or
    a refusal detail dict describing the first mismatching source.
    """
    identity_head = cfg.identity_head
    if identity_head is None:
        identity_head = cfg.head if cfg.head is not None else _git_head(cfg.repo)
    current = {"head": identity_head, "binary_sha256": _safe_digest(Path(cfg.binary))}
    sources = []
    for name, filename, fields in (
        ("identity.json", "identity.json", {"head": "head", "binary_sha256": "binary_sha256"}),
        ("metadata.json", "metadata.json", {"head": "head", "binary_sha256": "binary_sha256"}),
        ("baseline-lock.json", "baseline-lock.json", {"head": "head", "binary_sha256": "runtime_binary_sha256"}),
    ):
        path = campaign_dir / filename
        if path.exists():
            data = _read_json_file(path)
            if isinstance(data, dict):
                sources.append((name, data, fields))
    for name, data, fields in sources:
        mismatches = {}
        for current_key, stored_key in fields.items():
            stored = data.get(stored_key)
            if stored is not None and stored != current[current_key]:
                mismatches[stored_key] = {"recorded": stored, "current": current[current_key]}
        if mismatches:
            return {"source": name, "mismatches": mismatches, "current": current}
    identity_path = campaign_dir / "identity.json"
    if not identity_path.exists():
        _write_json(identity_path, {"schema": 1, **current})
    return None


def _default_prompt(py: str) -> str:
    return f"""Read TASK.md and implement the complete Incremental Build and Publication Platform upgrade in the existing v1 workspace. Use the current language and standard library/dependencies only. Run the immutable public tests first with process.run using executable {py} and arguments -m unittest discover -s tests -v. Implement strict CSV/JSONL validation, deterministic DAG operators and independent reference checks, content/config fingerprints, immutable CAS manifests and atomic GC, 4-8 worker processes with leases/stale-token fencing/retries, a durable outbox with idempotent localhost publication and receipt reconciliation, an interrupted v1-to-v2 migration, CLI/API, README, DESIGN, recovery runbook and RESULT.json. Add meaningful app tests for independent process contention and crashes under app/tests; do not modify TASK.md, tests, fixtures or oracle.py. Never execute payloads. Work only in app/ and be honest about incomplete work; ordinary final is not operator acceptance. Preserve all current requirements across later corrections."""


def _resolve_prompt(cfg: RunnerConfig, campaign_dir: Path, py: str) -> str:
    if cfg.prompt_text is not None:
        return cfg.prompt_text
    if cfg.prompt_file:
        path = Path(cfg.prompt_file)
        if not path.is_absolute():
            path = Path(campaign_dir) / path
        return path.read_text(encoding="utf-8").replace("{python}", py)
    return _default_prompt(py)


def _default_child_command(cfg: RunnerConfig, work_dir: Path, out_dir: Path) -> list:
    action = (
        ["--work", "--prompt=-"]
        if cfg.mode == "work"
        else (["--restore=latest", "--continue"] if cfg.mode == "resume" else ["--restore=latest", "--prompt=-"])
    )
    return [
        str(cfg.binary),
        *action,
        "--context=dynamic",
        f"--max-rounds={cfg.rounds}",
        "--timeout-secs=720",
        f"--grant-file={out_dir / 'grants.json'}",
        f"--jsonl-out={out_dir / 'events.jsonl'}",
        str(work_dir),
    ]


def _spawn_child(command, cwd: Path, env: dict, stdout_handle, stderr_handle) -> subprocess.Popen:
    kwargs = {}
    if WINDOWS:
        # own process group so the graceful CTRL_BREAK only reaches our tree
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    try:
        return subprocess.Popen(
            command,
            cwd=str(cwd),
            env=env,
            stdin=subprocess.PIPE,
            stdout=stdout_handle,
            stderr=stderr_handle,
            text=True,
            encoding="utf-8",
            **kwargs,
        )
    except OSError as error:
        raise ChildLaunchError(f"failed to spawn {command[0]!r}: {error}") from error


def _stop_child(process: subprocess.Popen, graceful_timeout_s: float, kill_timeout_s: float) -> dict:
    """Bounded graceful stop, then kill only this child's process tree.

    Windows: taskkill /T /F restricted to our own pid (never unrelated
    processes), with process.kill() as fallback. POSIX: SIGINT, then SIGKILL
    to the child's own process group.
    """
    result = {"graceful_signal": None, "taskkill_exit": None, "outcome": "unconfirmed"}
    if process.poll() is not None:
        result["outcome"] = "already_exited"
        return result
    try:
        if WINDOWS:
            process.send_signal(signal.CTRL_BREAK_EVENT)
            result["graceful_signal"] = "CTRL_BREAK_EVENT"
        else:
            process.send_signal(signal.SIGINT)
            result["graceful_signal"] = "SIGINT"
    except (OSError, ValueError):
        pass
    try:
        process.wait(timeout=max(graceful_timeout_s, 0.0))
        result["outcome"] = "terminated_confirmed"
        result["graceful"] = True
        return result
    except subprocess.TimeoutExpired:
        pass
    killed = False
    if WINDOWS:
        try:
            completed = subprocess.run(
                ["taskkill", "/T", "/F", "/PID", str(process.pid)],
                capture_output=True,
                text=True,
                timeout=max(kill_timeout_s, 1.0),
            )
            result["taskkill_exit"] = completed.returncode
            killed = completed.returncode == 0
        except (OSError, subprocess.TimeoutExpired) as error:
            result["taskkill_error"] = repr(error)
    else:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
            killed = True
        except OSError as error:
            result["killpg_error"] = repr(error)
    if not killed:
        try:
            process.kill()
            killed = True
        except OSError:
            pass
    try:
        process.wait(timeout=max(kill_timeout_s, 0.0))
        result["outcome"] = "terminated_confirmed"
    except subprocess.TimeoutExpired:
        result["outcome"] = "unconfirmed"
    return result


def _wait_for_child(process: subprocess.Popen, prompt, timeout_s: float, interrupt_event, poll_s: float = 0.1) -> str:
    """Feed the prompt and wait, checking for timeout and interrupts.

    Popen.communicate(timeout=...) never kills the child on timeout, so the
    wait is owned here: on timeout the caller performs the bounded graceful
    stop and tree kill itself. KeyboardInterrupt (real SIGINT or the injected
    event) propagates into the shared finalization.
    """
    if prompt is not None:
        def _feed():
            try:
                process.stdin.write(prompt)
                process.stdin.flush()
            except (OSError, ValueError):
                pass
            finally:
                try:
                    process.stdin.close()
                except (OSError, ValueError):
                    pass

        threading.Thread(target=_feed, daemon=True, name="runner-stdin").start()
    deadline = time.monotonic() + max(timeout_s, 0.0)
    while True:
        if interrupt_event is not None and interrupt_event.is_set():
            raise KeyboardInterrupt
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return "timeout"
        try:
            process.wait(timeout=min(poll_s, remaining))
            return "completed"
        except subprocess.TimeoutExpired:
            continue


@dataclass
class _RunState:
    out_dir: Path
    started: float = field(default_factory=time.monotonic)
    outcome: str = "running"
    error: str | None = None
    refusal: dict | None = None
    process: subprocess.Popen | None = None
    relay: _Relay | None = None
    ledger: BudgetLedger | None = None
    ledger_inherited: bool = False
    segment_created: bool = False
    child_exit_code: int | None = None
    child_command: list | None = None
    protected_before: dict | None = None
    metadata_initial: dict = field(default_factory=dict)
    cleanup: dict = field(default_factory=dict)
    categories: list = field(default_factory=list)
    exit_code: int = EXIT_INTERNAL_ERROR


def _prepare_segment_files(cfg: RunnerConfig, state: _RunState, campaign_dir: Path, work_dir: Path, env: dict) -> None:
    out_dir = state.out_dir
    py = (env.get("AGENT_PYTHON") if env else None) or sys.executable
    grants = [
        {
            "id": "app-write",
            "risk": "WorkspaceWrite",
            "target": {"workspace_path_prefix": "app"},
            "constraint": {"max_content_bytes": 160000},
            "expires_at_ms": int((time.time() + 1800) * 1000),
        },
        {
            "id": "python-tests",
            "risk": "ProcessExecution",
            "target": {"exec_argv_prefix": [py]},
            "constraint": {"max_runs": 48},
            "expires_at_ms": int((time.time() + 1800) * 1000),
        },
    ]
    (out_dir / "grants.json").write_text(json.dumps(grants), encoding="utf-8")
    prompt = _resolve_prompt(cfg, campaign_dir, py)
    (out_dir / "prompt.txt").write_text(prompt, encoding="utf-8")
    dirty_diff_sha256 = None
    if cfg.capture_git_diff:
        try:
            dirty = subprocess.check_output(["git", "diff", "--binary"], cwd=str(cfg.repo))
            (out_dir / "runtime.patch").write_bytes(dirty)
            dirty_diff_sha256 = hashlib.sha256(dirty).hexdigest()
        except (OSError, subprocess.CalledProcessError):
            dirty_diff_sha256 = None
    head = cfg.head if cfg.head is not None else _git_head(cfg.repo)
    state.protected_before = _protected_hashes(work_dir, cfg.protected_files)
    state.metadata_initial = {
        "head": head,
        "dirty_diff_sha256": dirty_diff_sha256,
        "binary_sha256": _safe_digest(cfg.binary),
        "protected_before": state.protected_before,
        "round_cap": cfg.rounds,
        "max_output_tokens": cfg.max_output_tokens,
        "cost_cap_usd": cfg.max_cost_usd,
        "model": env.get("OPENAI_MODEL") if env else None,
        "mode": cfg.mode,
        "wire_capture": "forwarding relay; credentials omitted",
        "outcome": "running",
        "campaign_dir": str(campaign_dir),
        "segment": cfg.segment,
        "ledger_path": str(state.ledger.path),
        "reserve_policy": state.ledger.data["reserve_policy"],
    }
    _write_json(out_dir / "metadata.json", state.metadata_initial)


def _finalize(cfg: RunnerConfig, state: _RunState, campaign_dir: Path, work_dir: Path) -> None:
    # one unconditional finalization: relay first (stop accepting), then the
    # child (bounded graceful stop, then tree kill), then reap and record.
    cleanup = {"order": []}
    if state.relay is not None:
        try:
            state.relay.close()
            cleanup["order"].append("relay_accept_stopped")
        except Exception as error:
            cleanup["relay_error"] = repr(error)
    if state.process is not None:
        try:
            cleanup["child"] = _stop_child(state.process, cfg.child_graceful_timeout_s, cfg.child_kill_timeout_s)
        except Exception as error:
            cleanup["child"] = {"outcome": "unconfirmed", "error": repr(error)}
        cleanup["order"].append("child_stop:" + cleanup["child"].get("outcome", "unconfirmed"))
        if state.child_exit_code is None and state.process.returncode is not None:
            state.child_exit_code = state.process.returncode
    if state.ledger is not None:
        try:
            cleanup["forced_unknown_attempts"] = state.ledger.force_settle_segment(
                cfg.segment, detail="forced_unknown_at_cleanup"
            )
        except Exception as error:
            cleanup["ledger_error"] = repr(error)
    state.cleanup = cleanup

    categories = set(state.refusal["categories"]) if state.refusal else set()
    outcome = state.refusal["outcome"] if state.refusal else state.outcome
    outcome_categories = {
        "launch_failed": "child_launch_failed",
        "interrupted": "interrupted",
        "child_wait_timeout": "child_wait_timeout",
        "internal_error": "internal_error",
        "budget_ledger_unreadable": "budget_ledger_unreadable",
        "invalid_config": "invalid_config",
    }
    if outcome in outcome_categories:
        categories.add(outcome_categories[outcome])
    child_cleanup = cleanup.get("child") or {}
    if child_cleanup.get("outcome") == "unconfirmed":
        categories.add("cleanup_unconfirmed")
    if state.outcome == "completed" and state.child_exit_code:
        categories.add("child_nonzero")
    protected_after = _protected_hashes(work_dir, cfg.protected_files)
    protected_unchanged = state.protected_before is not None and protected_after == state.protected_before
    if not protected_unchanged and state.segment_created:
        categories.add("protected_modified")
    event_rows = []
    events_path = state.out_dir / "events.jsonl"
    if events_path.exists():
        try:
            event_rows = [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
        except (OSError, json.JSONDecodeError):
            event_rows = []
    terminals = [row for row in event_rows if isinstance(row, dict) and row.get("type") in ("turn_failed", "turn_completed", "recovery_required")]
    if any(row.get("type") == "recovery_required" for row in terminals):
        categories.add("recovery_required")
    snap = state.ledger.snapshot() if state.ledger is not None else None
    if snap is not None and (snap["unknown_usd"] > 0.0 or snap["cap_stopped"]):
        categories.add("budget_incomplete")
    state.categories = sorted(categories)
    state.exit_code = exit_code_for(categories)

    if not state.segment_created:
        receipt = {
            "status": outcome,
            "categories": state.categories,
            "exit_code": state.exit_code,
            "detail": (state.refusal or {}).get("detail"),
            "head": cfg.identity_head if cfg.identity_head is not None else cfg.head,
            "binary_sha256": _safe_digest(cfg.binary),
            "campaign_dir": str(campaign_dir),
            "segment": cfg.segment,
        }
        refusals = Path(campaign_dir) / "refusals"
        _write_json(refusals / f"{cfg.segment}-{int(time.time())}.json", receipt)
        _print_json({"status": outcome, "exit_code": state.exit_code, "categories": state.categories, "detail": (state.refusal or {}).get("detail")})
        return

    out_dir = state.out_dir
    requests = len(state.relay.wire) if state.relay is not None else 0
    elapsed = round(time.monotonic() - state.started, 2)
    budget_receipt = {
        "cap_usd": cfg.max_cost_usd,
        "inherited_ledger": state.ledger_inherited,
        "cap_stopped": snap["cap_stopped"] if snap else False,
        "committed_usd": snap["committed_usd"] if snap else 0.0,
        "reserved_usd": snap["reserved_usd"] if snap else 0.0,
        "unknown_usd": snap["unknown_usd"] if snap else 0.0,
        "reserve_policy": snap["reserve_policy"] if snap else {},
    }
    usage_receipt = {
        "cost_cap_usd": cfg.max_cost_usd,
        "amounts_are_estimates": True,
        "settled_committed_usd": snap["committed_usd"] if snap else 0.0,
        "open_reserved_usd": snap["reserved_usd"] if snap else 0.0,
        "unknown_usd": snap["unknown_usd"] if snap else 0.0,
        "estimated_peak_usd": (snap["committed_usd"] + snap["unknown_usd"]) if snap else 0.0,
        "usage_known": bool(snap and snap["unknown_usd"] == 0.0 and not snap["cap_stopped"]),
        "cap_stopped": snap["cap_stopped"] if snap else False,
        "ledger_path": str(state.ledger.path) if state.ledger else None,
        "ledger_inherited": state.ledger_inherited,
        "reserve_policy": snap["reserve_policy"] if snap else {},
        "rows": [row for row in (snap["attempts"] if snap else []) if row.get("segment") == cfg.segment],
    }
    metadata = dict(state.metadata_initial)
    metadata.update(
        {
            "outcome": outcome,
            "exit": state.child_exit_code,
            "child_exit_code": state.child_exit_code,
            "exit_code": state.exit_code,
            "categories": state.categories,
            "elapsed": elapsed,
            "requests": requests,
            "protected_after": protected_after,
            "protected_unchanged": protected_unchanged,
            "cleanup": cleanup,
            "budget": budget_receipt,
        }
    )
    if state.error:
        metadata["error"] = state.error
    if outcome == "launch_failed":
        metadata["launch_error"] = state.error
    if state.child_command is not None:
        metadata["child_command"] = state.child_command
    summary = {
        "outcome": outcome,
        "categories": state.categories,
        "exit_code": state.exit_code,
        "exit": state.child_exit_code,
        "child_exit_code": state.child_exit_code,
        "elapsed": elapsed,
        "rounds": sum(row.get("type") == "model_started" for row in event_rows if isinstance(row, dict)),
        "tool_calls": sum(row.get("type") == "tool_finished" for row in event_rows if isinstance(row, dict)),
        "protected_unchanged": protected_unchanged,
        "terminals": terminals,
        "session": event_rows[-1] if event_rows else None,
        "requests": requests,
        "budget": budget_receipt,
        "cleanup": cleanup,
    }
    # receipts land in this order so at least one complete terminal record
    # survives even if a later write is interrupted
    _write_json(out_dir / "usage-ledger.json", usage_receipt)
    _write_json(out_dir / "metadata.json", metadata)
    _write_json(out_dir / "summary.json", summary)
    _print_json(summary)


def run_segment(cfg: RunnerConfig) -> int:
    """Run one segment and return this process's layered exit code."""
    _validate_config(cfg)
    # The spawned child runs with cwd=work_dir, so every path handed to it
    # (grant file, jsonl out, workspace) must be absolute no matter how the
    # caller spelled --campaign-dir.
    campaign_dir = Path(cfg.campaign_dir).resolve()
    work_dir = Path(cfg.work_dir).resolve() if cfg.work_dir is not None else campaign_dir / "workspace"
    out_dir = campaign_dir / cfg.segment
    ledger_path = Path(cfg.ledger_path).resolve() if cfg.ledger_path is not None else campaign_dir / "budget-ledger.json"
    state = _RunState(out_dir=out_dir)
    try:
        mismatch = _verify_campaign_identity(cfg, campaign_dir)
        if mismatch is not None:
            state.refusal = {
                "outcome": "identity_mismatch",
                "categories": ["identity_mismatch"],
                "detail": mismatch,
            }
            return EXIT_IDENTITY_MISMATCH
        try:
            state.ledger, state.ledger_inherited = BudgetLedger.load(
                ledger_path, cfg.max_cost_usd, cfg.pricing, cfg.max_output_tokens
            )
        except _BudgetLedgerError as error:
            state.refusal = {
                "outcome": "budget_ledger_unreadable",
                "categories": ["budget_ledger_unreadable"],
                "detail": str(error),
            }
            return EXIT_BUDGET_INCOMPLETE
        env = dict(cfg.env) if cfg.env is not None else build_env(cfg.repo)
        upstream = cfg.upstream_url or env.get("OPENAI_BASE_URL")
        if not upstream:
            raise ConfigError("OPENAI_BASE_URL missing from environment; cannot forward requests")
        try:
            out_dir.mkdir(parents=False)
        except FileExistsError:
            state.refusal = {
                "outcome": "segment_exists",
                "categories": ["segment_exists"],
                "detail": f"segment directory already exists: {out_dir}",
            }
            return EXIT_SEGMENT_EXISTS
        state.segment_created = True
        _prepare_segment_files(cfg, state, campaign_dir, work_dir, env)
        state.relay = _Relay(
            upstream=upstream,
            ledger=state.ledger,
            out_dir=out_dir,
            segment=cfg.segment,
            upstream_timeout_s=cfg.upstream_timeout_s,
        )
        env.update(
            OPENAI_BASE_URL=f"{state.relay.base_url}/v1",
            OPENAI_API_PROTOCOL="responses",
            OPENAI_RESPONSES_REASONING_EFFORT="none",
            OPENAI_MAX_OUTPUT_TOKENS=str(cfg.max_output_tokens),
            OPENAI_PROMPT_CACHE_MODE="provider_default",
            OPENAI_RETRY_METRICS_FILE=str(out_dir / "retry.jsonl"),
        )
        command = cfg.child_command if cfg.child_command is not None else _default_child_command(cfg, work_dir, out_dir)
        state.child_command = [str(part) for part in command]
        prompt = None if cfg.mode == "resume" else _resolve_prompt(cfg, campaign_dir, (env.get("AGENT_PYTHON") or sys.executable))
        with (out_dir / "stderr.log").open("w", encoding="utf-8") as error_handle, (out_dir / "stdout.log").open("w", encoding="utf-8") as stdout_handle:
            state.process = _spawn_child(command, work_dir, env, stdout_handle, error_handle)
        (out_dir / "pid.txt").write_text(str(state.process.pid), encoding="utf-8")
        wait_outcome = _wait_for_child(state.process, prompt, cfg.child_wait_timeout_s, cfg.interrupt_event)
        if state.process.returncode is not None:
            state.child_exit_code = state.process.returncode
        state.outcome = "completed" if wait_outcome == "completed" else "child_wait_timeout"
    except KeyboardInterrupt:
        state.outcome = "interrupted"
    except ChildLaunchError as error:
        state.outcome = "launch_failed"
        state.error = str(error)
    except ConfigError as error:
        state.refusal = {
            "outcome": "invalid_config",
            "categories": ["invalid_config"],
            "detail": str(error),
        }
    except Exception as error:  # unexpected, but receipts must still land
        if state.outcome == "running":
            state.outcome = "internal_error"
        state.error = repr(error)
    finally:
        _finalize(cfg, state, campaign_dir, work_dir)
    return state.exit_code


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Bounded real-model runner for the incremental-platform campaign.")
    parser.add_argument("segment")
    parser.add_argument("--campaign-dir", type=Path, default=CAMPAIGN)
    parser.add_argument("--mode", choices=["work", "resume", "feedback"], default="work")
    parser.add_argument("--prompt-file")
    parser.add_argument("--rounds", type=int, default=24)
    parser.add_argument("--max-cost-usd", type=float, default=1.5)
    parser.add_argument("--max-output-tokens", type=int, default=8192)
    args = parser.parse_args(argv)
    cfg = RunnerConfig(
        segment=args.segment,
        campaign_dir=args.campaign_dir,
        mode=args.mode,
        prompt_file=args.prompt_file,
        rounds=args.rounds,
        max_cost_usd=args.max_cost_usd,
        max_output_tokens=args.max_output_tokens,
        env=None,
    )
    try:
        return run_segment(cfg)
    except ConfigError as error:
        _print_json({"status": "invalid_arguments", "error": str(error)})
        return EXIT_USAGE


if __name__ == "__main__":
    raise SystemExit(main())

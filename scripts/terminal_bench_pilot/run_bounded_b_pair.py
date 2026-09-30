"""Run a separately named bounded live-task pair without exposing the key.

Every invocation needs an explicit identity, limits, and binary digest. The
selection lock is an upper envelope, not a grant or a pricing guarantee.
"""
from __future__ import annotations

import argparse
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import urllib.request
from zoneinfo import ZoneInfo

from terminal_bench_pilot.credential_relay import CredentialRelay
from terminal_bench_pilot.outcome import summarize_trial
from runtime_endurance_incremental_runner import build_env


LOCKED_ROUNDS = 400
LOCKED_ATTEMPTS = 460
LOCKED_INPUT_TOKENS = 16_000_000
LOCKED_OUTPUT_TOKENS = 440_000
LOCKED_AGENT_TIMEOUT = 14_400
LOCKED_ESTIMATED_USD = 4
LOCKED_TOOL_ATTEMPTS = 1_400
LIVE_WRITE_SCOPES = {
    "api", "_verify.py", "migrate.py", "entrypoint.sh", "migrate_archive",
}
PROVIDER_PROFILES = {
    "deepseek-flash": {
        "base_url": "https://api.deepseek.com/v1",
        "price_checked_date": "2026-09-24",
        "price_source": "https://api-docs.deepseek.com/quick_start/pricing/",
        "model_version": "DeepSeek-V4.1-Flash",
        "input_price_milli_usd_per_million": 300,
        "output_price_milli_usd_per_million": 1200,
        "api_safe_tool_names": False,
        "prefer_max_completion_tokens": False,
        "omit_stream_options": False,
        "min_request_interval_ms": 0,
    },
    "mimo-v2.6-flash": {
        "base_url": "https://api.xiaomimimo.com/v1",
        "price_checked_date": "2026-09-27",
        "price_source": "https://mimo.mi.com/docs/en-US/pricing",
        "model_version": "MiMo-V2.6-Flash",
        "input_price_milli_usd_per_million": 140,
        "output_price_milli_usd_per_million": 280,
        "api_safe_tool_names": True,
        "prefer_max_completion_tokens": True,
        "omit_stream_options": True,
        "min_request_interval_ms": 750,
        "main_request_timeout_secs": 600,
        "relay_upstream_timeout_secs": 540,
        "repair_missing_tool_call_ids": True,
        "declared_context_window": 65_536,
        "per_request_output_tokens": 16_384,
        "rpm_limit": 100,
        "tpm_limit": 10_000_000,
    },
    "pinaic-gpt-6-luna": {
        "base_url": "https://api.pinaic.com/v1",
        "model_id": "gpt-6-luna",
        "api_protocol": "responses",
        "price_checked_date": "2026-09-27",
        "price_source": "user-provided Pinaic tariff (2026-09-27)",
        "price_basis": "user-provided Pinaic rates; not independently checked against billing",
        "proxy_price_verified": False,
        "tariff_user_provided": True,
        "model_version": "GPT-6 Luna via Pinaic; requested reasoning effort max",
        "responses_reasoning_effort": "max",
        "input_price_milli_usd_per_million": 400,
        "cached_input_price_milli_usd_per_million": 40,
        "output_price_milli_usd_per_million": 2_000,
        "api_safe_tool_names": True,
        "prefer_max_completion_tokens": False,
        "omit_stream_options": False,
        "chat_thinking": None,
        "min_request_interval_ms": 1_000,
        "main_request_timeout_secs": 600,
        "relay_upstream_timeout_secs": 540,
        "repair_missing_tool_call_ids": False,
        "declared_context_window": 65_536,
        "per_request_output_tokens": 16_384,
    },
    "easycli-local-gpt-6-luna": {
        "base_url": "http://127.0.0.1:8317/v1",
        "model_id": "gpt-6-luna",
        "api_protocol": "responses",
        "auth_mode": "unauthenticated_loopback",
        "price_checked_date": "2026-09-30",
        "price_source": "https://developers.openai.com/api/docs/models/gpt-6-luna",
        "price_basis": (
            "OpenAI direct API list-price equivalent only; EasyCLI Codex Plus "
            "subscription quota and actual billing are not measured by this estimate"
        ),
        "proxy_price_verified": False,
        "tariff_user_provided": False,
        "subscription_quota": True,
        "model_version": "GPT-6 Luna via local EasyCLIProxyAPI; reasoning effort max",
        "responses_reasoning_effort": "max",
        "input_price_milli_usd_per_million": 100,
        "cached_input_price_milli_usd_per_million": 10,
        "output_price_milli_usd_per_million": 500,
        "api_safe_tool_names": True,
        "prefer_max_completion_tokens": False,
        "omit_stream_options": False,
        "min_request_interval_ms": 1_000,
        # Let the local proxy's observed ~600s stream cutoff reach the
        # client before its own deadline, so a 408 is not masked by a
        # downstream BrokenPipeError. Both waits remain finite.
        "main_request_timeout_secs": 660,
        "relay_upstream_timeout_secs": 600,
        "max_retryable_408_retries": 1,
        "repair_missing_tool_call_ids": False,
        "declared_context_window": 65_536,
        "per_request_output_tokens": 16_384,
    },
}
TASK_IMAGE = "harborframework/terminal-bench:live-database-cutover-environment-5997c5b486ffe2b0"
TASK_GOAL = (
    "Solve the official task in /app and preserve every requirement. Inspect all "
    "modules and declared interfaces before editing. After each API or schema "
    "change, run bounded import and health checks, including with MySQL settings "
    "absent, and keep repairing errors until a fresh MySQL-less verifier startup "
    "is viable. Leave the complete implementation in the workspace and only "
    "report completion after the required checks actually pass."
)
TASK_SOURCE_BASE = (
    "https://raw.githubusercontent.com/harbor-framework/terminal-bench/"
    "452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/live-database-cutover/"
)


def validate_process_grant(
    grant_file: Path, rounds: int, min_expires_at_ms: int | None = None,
    expected_write_scopes: set[str] | None = None,
) -> None:
    """Reject a window that cannot sustain even one Python run per decision.

    This is an admission lower bound, not a request to enlarge authority or
    a prediction of how many process effects the model will need.
    """
    if rounds <= 0:
        raise ValueError("rounds must be positive")
    grants = json.loads(grant_file.read_text(encoding="utf-8"))
    if expected_write_scopes is not None:
        write_scopes = {
            grant.get("target", {}).get("workspace_path_prefix")
            for grant in grants if grant.get("risk") == "WorkspaceWrite"
        }
        if write_scopes != expected_write_scopes:
            raise RuntimeError(
                "workspace-write scopes differ from the declared live-task harness boundary"
            )
    python_grants = [
        grant for grant in grants
        if grant.get("risk") == "ProcessExecution"
        and grant.get("target", {}).get("exec_argv_prefix") == ["python3"]
    ]
    if len(python_grants) != 1:
        raise RuntimeError("expected one explicit python3 execution grant")
    available = python_grants[0].get("constraint", {}).get("max_runs")
    if type(available) is not int or available < rounds:
        raise RuntimeError(
            f"python3 grant permits {available!r} runs for {rounds} model decisions; "
            "provide a separately authorized, bounded trial grant"
        )
    if min_expires_at_ms is not None:
        for grant in grants:
            expiry = grant.get("expires_at_ms")
            if type(expiry) is not int or expiry <= min_expires_at_ms:
                raise RuntimeError("trial grant expires before the bounded agent window ends")


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _clean_environment(
    repo: Path, relay: CredentialRelay, model: str, api_protocol: str = "chat",
) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith((
            "OPENAI_", "DEEPSEEK_", "ANTHROPIC_", "AZURE_OPENAI_",
            "GOOGLE_", "GEMINI_", "MIMO_", "XIAOMI_",
            "TB_RELAY_", "MAINTENANCE_",
        ))
    }
    env.update(
        OPENAI_MODEL=model,
        OPENAI_API_PROTOCOL=api_protocol,
        TB_RELAY_BASE_URL=relay.base_url,
        TB_RELAY_TOKEN=relay.token,
        PYTHONPATH=str(repo / "scripts"),
    )
    return env


def _verify_task_identity(repo: Path, task: Path) -> dict[str, str]:
    lock = json.loads(
        (repo / "docs/experiments/terminal-bench-pilot/selection.lock.json").read_text(
            encoding="utf-8"
        )
    )
    locked = next(item for item in lock["tasks"] if item["id"] == "live-database-cutover")
    expected = {
        item["path"]: item["sha256"]
        for item in locked["source_files"]
        if item.get("sha256")
    }
    local = {name: _sha256(task / name) for name in expected}
    if local.get("instruction.md") != expected.get("instruction.md"):
        raise RuntimeError("downloaded instruction.md differs from the locked source")
    official_task_toml = urllib.request.urlopen(
        TASK_SOURCE_BASE + "task.toml", timeout=30
    ).read()
    official_hash = hashlib.sha256(official_task_toml).hexdigest()
    if official_hash != expected.get("task.toml"):
        raise RuntimeError("official task.toml at the locked commit differs from selection.lock")
    # Harbor normalizes environment/verifier fields in the local package. Keep
    # both identities visible instead of silently treating that rewrite as the
    # upstream source hash.
    return {
        "instruction.md": local["instruction.md"],
        "task.toml_harbor_normalized": local["task.toml"],
        "task.toml_official_raw": official_hash,
    }


def _verify_binary_in_task_image(binary: Path, image_id: str) -> dict[str, str]:
    """Run only the checked ELF's help path in the exact offline task image."""
    command = [
        "docker", "run", "--rm", "--pull=never", "--network", "none",
        "--read-only", "--mount",
        f"type=bind,src={binary},dst=/opt/context-agent/bin/agent-tui,readonly",
        "--entrypoint", "/opt/context-agent/bin/agent-tui", image_id, "--help",
    ]
    try:
        completed = subprocess.run(
            command, capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=30, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("trial binary startup could not be verified in task image") from error
    if completed.returncode != 0 or "--state-dir=" not in (completed.stdout or ""):
        # A loader or CLI error may echo provider data; keep only the category.
        raise RuntimeError("trial binary cannot start with required CLI in task image")
    return {"status": "PASS", "task_image_id": image_id,
            "required_help_flag": "--state-dir="}


def _relay_smoke(relay: CredentialRelay, env: dict[str, str]) -> None:
    probe = (
        "import urllib.request,urllib.error;\n"
        "try:\n"
        f" urllib.request.urlopen(urllib.request.Request('{relay.base_url}/probe', data=b'x'), timeout=10)\n"
        "except urllib.error.HTTPError as e:\n"
        " assert e.code == 404\n"
    )
    smoke_name = "context-agent-bounded-relay-smoke"
    try:
        subprocess.run(
            [
                "docker", "run", "--rm", "--name", smoke_name,
                "--network", "bridge", "--entrypoint", "python3", TASK_IMAGE,
                "-c", probe,
            ],
            env=env,
            check=True,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    finally:
        subprocess.run(
            ["docker", "rm", "-f", smoke_name],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=20,
        )


class JobEventMonitor:
    """Observe the current Harbor job without copying tool/model bodies."""

    def __init__(self, job_root: Path, tool_cap: int):
        self.job_root = job_root
        self.tool_cap = tool_cap
        self.offsets: dict[Path, int] = {}
        self.tool_attempts = 0
        self.stop_reason: str | None = None

    def scan(self) -> str | None:
        for path in self.job_root.glob("live-database-cutover__*/agent/context-agent/events.jsonl"):
            with path.open("rb") as stream:
                stream.seek(self.offsets.get(path, 0))
                while True:
                    position = stream.tell()
                    line = stream.readline()
                    if not line:
                        break
                    if not line.endswith(b"\n"):
                        stream.seek(position)
                        break
                    self.offsets[path] = stream.tell()
                    try:
                        event = json.loads(line).get("event", {})
                    except (ValueError, AttributeError):
                        continue
                    if event.get("type") == "tool_started":
                        self.tool_attempts += 1
                        if self.tool_attempts >= self.tool_cap:
                            self.stop_reason = "ToolAttemptBudgetReached"
                    if event.get("type") == "tool_finished" and (
                        (event.get("output", {}).get("metadata") or {}).get("failure_class")
                        == "approval_denied"
                    ):
                        self.stop_reason = "ApprovalDeniedEarlyStop"
        return self.stop_reason


def run_harbor_bounded(
    harbor: str, config_path: Path, repo: Path, env: dict[str, str],
    log_path: Path, job_root: Path, timeout_secs: int, tool_cap: int,
) -> tuple[int, str | None, int]:
    monitor = JobEventMonitor(job_root, tool_cap)
    reason: str | None = None
    deadline = time.monotonic() + timeout_secs + 900
    with log_path.open("w", encoding="utf-8") as log:
        process = subprocess.Popen(
            [harbor, "run", "--config", str(config_path)], cwd=repo, env=env,
            stdout=log, stderr=subprocess.STDOUT,
        )
        try:
            while process.poll() is None:
                reason = monitor.scan()
                if reason is not None:
                    process.send_signal(signal.SIGINT)
                    break
                if time.monotonic() >= deadline:
                    reason = "TimeoutExpired"
                    process.send_signal(signal.SIGINT)
                    break
                time.sleep(0.5)
        except KeyboardInterrupt:
            reason = "InterruptedByOperator"
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=45 if reason else None)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=15)
            reason = reason or "HarborCleanupTimeout"
    monitor.scan()
    return process.returncode, reason or monitor.stop_reason, monitor.tool_attempts


def relay_usage_settled(relay: CredentialRelay) -> bool:
    """Whether every admitted provider attempt settled or used its exact 408 recovery."""
    if relay.active_calls > 0:
        return False
    attempts = relay.attempts
    unresolved = [attempt for attempt in attempts if attempt.get("state") != "completed"]
    if not unresolved:
        return relay.input_tokens_reserved == 0 and relay.output_tokens_reserved == 0
    recovered_retries = [
        attempt for attempt in attempts
        if attempt.get("state") == "completed" and attempt.get("retry_of_unknown_408")
    ]
    return (
        len(unresolved) == 1
        and unresolved[0].get("retryable_upstream_408") is True
        and relay.retryable_408_recovered
        and bool(recovered_retries)
    )


def comparison_status(result: dict, provider_usage_settled: bool | None = None) -> str:
    """Classify whether a completed arm can enter the paired comparison."""
    if result.get("runner_error") is not None:
        return "RUNNER_STOPPED"
    if provider_usage_settled is False:
        return "USAGE_UNKNOWN"
    if result.get("harbor_exit") != 0:
        return "HARBOR_NONZERO"
    trials = result.get("trials") or []
    if len(trials) != 1:
        return "MISSING_OR_EXTRA_TRIAL"
    trial = trials[0]
    if (trial.get("runtime") or {}).get("status") != "completed":
        return "RUNTIME_NOT_COMPLETED"
    if trial.get("harbor_exception") is not None:
        return "HARBOR_EXCEPTION"
    if trial.get("grading_status") != "GRADED":
        return "NO_OFFICIAL_GRADE"
    return "COMPARABLE"


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report-name", required=True)
    parser.add_argument("--protocol", required=True)
    parser.add_argument("--provider", choices=sorted(PROVIDER_PROFILES), required=True)
    parser.add_argument("--credential-stdin", action="store_true")
    parser.add_argument("--standard-cyber-safeguards", action="store_true",
                        help="set Responses access_programs.cyber=standard in the relay")
    parser.add_argument("--single-file-edit-patches", action="store_true",
                        help="expose edit.patch with strict files.maxItems=1 for this trial")
    parser.add_argument("--arms", choices=("pair", "dynamic", "rolling"), default="pair")
    parser.add_argument("--rounds", type=int, required=True)
    parser.add_argument("--attempts", type=int, required=True)
    parser.add_argument("--input-token-cap", type=int, required=True)
    parser.add_argument("--output-token-cap", type=int, required=True)
    parser.add_argument("--peak-miss-usd-cap", type=int, required=True)
    parser.add_argument("--timeout-secs", type=int, required=True)
    parser.add_argument("--expected-binary-sha256", required=True)
    parser.add_argument("--binary-path", type=Path, required=True)
    parser.add_argument("--grant-file", type=Path, required=True)
    parser.add_argument("--expected-grant-sha256", required=True)
    parser.add_argument("--task-dir", type=Path, required=True)
    parser.add_argument("--expected-image-sha256", required=True)
    parser.add_argument("--tool-attempt-cap", type=int, required=True)
    parser.add_argument("--preflight-only", action="store_true")
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{2,70}", args.report_name):
        parser.error("report-name must be a short lowercase hyphenated identity")
    if not re.fullmatch(r"TB[0-9]+_[A-Z0-9_]+", args.protocol):
        parser.error("protocol must be an explicit TB generation identifier")
    limits = (("rounds", LOCKED_ROUNDS), ("attempts", LOCKED_ATTEMPTS),
              ("input_token_cap", LOCKED_INPUT_TOKENS),
              ("output_token_cap", LOCKED_OUTPUT_TOKENS),
              ("peak_miss_usd_cap", LOCKED_ESTIMATED_USD),
              ("timeout_secs", LOCKED_AGENT_TIMEOUT))
    for name, maximum in limits:
        value = getattr(args, name)
        if not 0 < value <= maximum:
            parser.error(f"{name} must be within 1..{maximum}")
    if args.attempts < args.rounds:
        parser.error("provider attempts cannot be fewer than model decisions")
    if not 0 < args.tool_attempt_cap <= LOCKED_TOOL_ATTEMPTS:
        parser.error(f"tool_attempt_cap must be within 1..{LOCKED_TOOL_ATTEMPTS}")
    if not re.fullmatch(r"[0-9a-f]{64}", args.expected_binary_sha256):
        parser.error("expected binary digest must be SHA-256 hex")
    if args.provider == "mimo-v2.6-flash" and not args.credential_stdin:
        parser.error("MiMo trials require an in-memory credential from stdin")
    if args.provider == "easycli-local-gpt-6-luna" and args.credential_stdin:
        parser.error("the local EasyCLI profile must not read an account key from stdin")
    if args.standard_cyber_safeguards and (
        args.provider != "easycli-local-gpt-6-luna"
        or PROVIDER_PROFILES[args.provider].get("api_protocol") != "responses"
    ):
        parser.error("standard cyber safeguards are restricted to the local EasyCLI Responses profile")
    if args.single_file_edit_patches and (
        args.provider != "easycli-local-gpt-6-luna"
        or PROVIDER_PROFILES[args.provider].get("api_protocol") != "responses"
        or not PROVIDER_PROFILES[args.provider].get("api_safe_tool_names")
    ):
        parser.error("single-file edit.patch is restricted to the local EasyCLI Responses profile")
    for name in ("expected_grant_sha256", "expected_image_sha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", getattr(args, name)):
            parser.error(f"{name} must be SHA-256 hex")
    return args


def _provider_credentials(
    repo: Path, provider_profile: dict, model_id: str, api_protocol: str,
    credential_stdin: bool,
) -> dict[str, str]:
    if provider_profile.get("auth_mode") == "unauthenticated_loopback":
        if credential_stdin:
            raise RuntimeError("the local EasyCLI profile cannot use a supplied key")
        return {
            "OPENAI_BASE_URL": provider_profile["base_url"],
            "OPENAI_API_KEY": "",
            "OPENAI_MODEL": model_id,
            "OPENAI_API_PROTOCOL": api_protocol,
        }
    if credential_stdin:
        credential = sys.stdin.readline().rstrip("\r\n")
        if not credential or len(credential) > 512:
            raise RuntimeError("missing or invalid in-memory trial credential")
        credentials = {
            "OPENAI_BASE_URL": provider_profile["base_url"],
            "OPENAI_API_KEY": credential,
            "OPENAI_MODEL": model_id,
            "OPENAI_API_PROTOCOL": api_protocol,
        }
        if provider_profile.get("responses_reasoning_effort"):
            credentials["OPENAI_RESPONSES_REASONING_EFFORT"] = (
                provider_profile["responses_reasoning_effort"]
            )
        return credentials
    credentials = build_env(repo)
    if (credentials.get("OPENAI_API_PROTOCOL") != api_protocol
            or credentials.get("OPENAI_MODEL") != model_id):
        raise RuntimeError("the locked Chat provider profile is unavailable")
    return credentials


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    provider_profile = PROVIDER_PROFILES[args.provider]
    model_id = provider_profile.get("model_id", args.provider)
    api_protocol = provider_profile.get("api_protocol", "chat")
    if (datetime.now(ZoneInfo("Asia/Shanghai")).date().isoformat()
            != provider_profile["price_checked_date"]):
        raise RuntimeError("provider price snapshot is stale; reverify and issue a new protocol")
    repo = Path(__file__).resolve().parents[2]
    cache = Path.home() / ".cache/context-agent-terminal-bench"
    task = args.task_dir.resolve(strict=True)
    if not task.is_dir():
        raise RuntimeError("fixed live task cache is absent; download it before starting")
    binary = args.binary_path.resolve(strict=True)
    if not binary.is_relative_to(repo / "target-debian12"):
        raise RuntimeError("trial binary must be archived under target-debian12")
    grant = args.grant_file.resolve(strict=True)
    if not binary.is_file() or not grant.is_file():
        raise RuntimeError("Debian12 agent binary or checked grants are absent")
    if _sha256(binary) != args.expected_binary_sha256:
        raise RuntimeError("Debian12 agent binary differs from the frozen digest")
    if _sha256(grant) != args.expected_grant_sha256:
        raise RuntimeError("trial grant differs from the frozen digest")
    validate_process_grant(
        grant, args.rounds,
        min_expires_at_ms=int(datetime.now().timestamp() * 1000)
        + args.timeout_secs * 1000,
        expected_write_scopes=LIVE_WRITE_SCOPES,
    )

    task_hashes = _verify_task_identity(repo, task)
    if task_hashes["task.toml_harbor_normalized"] != (
        "771fb5d6703e63a80d9d468f21d8f090be9df95c2262461ecfe0ac318618a540"
    ):
        raise RuntimeError("Harbor-normalized task.toml differs from the checked package")
    gateway = json.loads(
        subprocess.check_output(["docker", "network", "inspect", "bridge"], text=True)
    )[0]["IPAM"]["Config"][0]["Gateway"]
    template = json.loads((cache / "tb7-r-live-job.json").read_text(encoding="utf-8"))
    if len(template.get("tasks", [])) != 1:
        raise RuntimeError("Harbor template must select exactly one task")
    template["tasks"][0]["path"] = str(task)
    mounts = template["environment"]["mounts"]
    binary_mounts = [mount for mount in mounts
                     if mount.get("target") == "/opt/context-agent/bin/agent-tui"]
    grant_mounts = [mount for mount in mounts
                    if mount.get("target") == "/etc/context-agent/grants.json"]
    if len(binary_mounts) != 1 or not binary_mounts[0].get("read_only"):
        raise RuntimeError("Harbor template has no unique read-only binary mount")
    binary_mounts[0]["source"] = str(binary)
    if len(grant_mounts) != 1 or not grant_mounts[0].get("read_only"):
        raise RuntimeError("Harbor template has no unique read-only grant mount")
    grant_mounts[0]["source"] = str(grant)
    image_id = subprocess.check_output(
        ["docker", "image", "inspect", TASK_IMAGE, "--format", "{{.Id}}"],
        text=True,
    ).strip()
    if image_id != "sha256:" + args.expected_image_sha256:
        raise RuntimeError("task environment image differs from the frozen digest")
    binary_compatibility = _verify_binary_in_task_image(binary, image_id)
    identity = {
        "protocol": args.protocol,
        "arms": ["dynamic", "rolling"] if args.arms == "pair" else [args.arms],
        "rounds_per_arm": args.rounds,
        "max_provider_attempts_per_arm": args.attempts,
        "max_input_tokens_per_arm": args.input_token_cap,
        "max_output_tokens_per_arm": args.output_token_cap,
        "tool_attempt_stop_threshold": args.tool_attempt_cap,
        "agent_timeout_secs": args.timeout_secs,
        "provider_profile": args.provider,
        "model": model_id,
        "provider_base_url": provider_profile["base_url"],
        "api_protocol": api_protocol,
        "responses_reasoning_effort": provider_profile.get("responses_reasoning_effort"),
        "standard_cyber_safeguards": args.standard_cyber_safeguards,
        "single_file_edit_patch": args.single_file_edit_patches,
        "edit_patch_files_max_per_call": 1 if args.single_file_edit_patches else 16,
        "api_safe_tool_names": provider_profile["api_safe_tool_names"],
        "prefer_max_completion_tokens":
            provider_profile["prefer_max_completion_tokens"],
        "omit_stream_options": provider_profile["omit_stream_options"],
        "min_request_interval_ms": provider_profile["min_request_interval_ms"],
        "main_request_timeout_secs": provider_profile.get("main_request_timeout_secs", 120),
        "relay_upstream_timeout_secs": provider_profile.get("relay_upstream_timeout_secs", 120),
        "max_retryable_408_retries": provider_profile.get("max_retryable_408_retries", 0),
        "repair_missing_tool_call_ids":
            provider_profile.get("repair_missing_tool_call_ids", False),
        "declared_context_window": provider_profile.get("declared_context_window", 32_768),
        "per_request_output_tokens": provider_profile.get("per_request_output_tokens", 8_192),
        "stream_retry_mode": "buffered_bounded",
        "excluded_tool_names": ["shell.exec"],
        "maintenance_calls": 0,
        "maintenance_tokens": 0,
        "harbor_retries": 0,
        "model_transport_max_attempts_per_call": 4,
        "model_transport_format_regenerations": 1,
        "peak_cache_miss_usd_cap_per_arm": args.peak_miss_usd_cap,
        "price_snapshot": {
            "checked_date": provider_profile["price_checked_date"],
            "source": provider_profile["price_source"],
            "model_version": provider_profile["model_version"],
            "basis": provider_profile.get("price_basis", "provider published price"),
            "proxy_price_verified": provider_profile.get("proxy_price_verified", True),
            "tariff_user_provided": provider_profile.get("tariff_user_provided", False),
            "subscription_quota": provider_profile.get("subscription_quota", False),
            "cache_miss_input_usd_per_million":
                provider_profile["input_price_milli_usd_per_million"] / 1000,
            "cache_read_input_usd_per_million": (
                provider_profile["cached_input_price_milli_usd_per_million"] / 1000
                if "cached_input_price_milli_usd_per_million" in provider_profile
                else None
            ),
            "output_usd_per_million":
                provider_profile["output_price_milli_usd_per_million"] / 1000,
            "max_token_envelope_peak_miss_usd": (
                provider_profile["input_price_milli_usd_per_million"]
                * args.input_token_cap
                + provider_profile["output_price_milli_usd_per_million"]
                * args.output_token_cap
            ) / 1_000_000_000,
            "limit_type": (
                "direct_api_equivalent_not_subscription_quota_cap"
                if provider_profile.get("subscription_quota") else
                "conservative_admission_estimate_not_provider_billing_hard_cap"
            ),
        },
        "provider_rate_limits": {
            "rpm": provider_profile.get("rpm_limit"),
            "tpm": provider_profile.get("tpm_limit"),
        },
        "task_path": str(task),
        "task_image": TASK_IMAGE,
        "task_image_id": image_id,
        "binary_compatibility": binary_compatibility,
        "task_files": task_hashes,
        "hashes": {
            str(path.relative_to(repo)): _sha256(path)
            for path in (
                binary,
                Path(__file__),
                Path(__file__).with_name("harbor_agent.py"),
                Path(__file__).with_name("credential_relay.py"),
                Path(__file__).with_name("read_probe.py"),
            )
        },
        "grant_sha256": _sha256(grant),
    }
    if args.preflight_only:
        print(json.dumps({"status": "STATIC_READY_NO_PROVIDER_CALLS",
                          "identity": identity}, indent=2), flush=True)
        return 0
    credentials = _provider_credentials(
        repo, provider_profile, model_id, api_protocol, args.credential_stdin,
    )
    report_dir = cache / args.report_name
    report_dir.mkdir(exist_ok=False)
    (report_dir / "identity.json").write_text(json.dumps(identity, indent=2), encoding="utf-8")
    all_results: list[dict] = []
    harbor = str(Path(sys.executable).with_name("harbor"))

    for arm in identity["arms"]:
        config = json.loads(json.dumps(template))
        name = f"{args.report_name}-{arm}"
        config["job_name"] = name
        config["agents"][0]["model_name"] = model_id
        config["agents"][0]["kwargs"].update({
            "context": arm,
            "task_goal": TASK_GOAL,
            "max_rounds": args.rounds,
            "timeout_secs": args.timeout_secs,
            "single_file_edit_patches": args.single_file_edit_patches,
        })
        config["agents"][0]["extra_allowed_hosts"] = [gateway]
        agent_env = {
            "OPENAI_API_PROTOCOL": api_protocol,
            "OPENAI_BUFFER_STREAM_FOR_RETRY": "1",
            "OPENAI_REQUEST_TIMEOUT_SECS": str(provider_profile.get("main_request_timeout_secs", 120)),
            "AGENT_DISABLE_SHELL_EXEC": "1",
            "OPENAI_MAX_OUTPUT_TOKENS": str(
                provider_profile.get("per_request_output_tokens", 8_192)
            ),
            "OPENAI_CONTEXT_WINDOW": str(
                provider_profile.get("declared_context_window", 32_768)
            ),
        }
        if provider_profile.get("chat_thinking"):
            agent_env["OPENAI_CHAT_THINKING"] = provider_profile["chat_thinking"]
        if provider_profile.get("responses_reasoning_effort"):
            agent_env["OPENAI_RESPONSES_REASONING_EFFORT"] = (
                provider_profile["responses_reasoning_effort"]
            )
        config["agents"][0]["env"] = agent_env
        config_path = report_dir / f"{arm}-config.json"
        config_path.write_text(json.dumps(config, indent=2), encoding="utf-8")
        relay = CredentialRelay(
            upstream=credentials["OPENAI_BASE_URL"],
            credential=credentials["OPENAI_API_KEY"],
            model=model_id,
            bind=gateway,
            max_requests=args.attempts,
            max_output_tokens=provider_profile.get("per_request_output_tokens", 8_192),
            max_input_tokens_total=args.input_token_cap,
            max_output_tokens_total=args.output_token_cap,
            max_peak_miss_usd=args.peak_miss_usd_cap,
            input_price_milli_usd_per_million=
                provider_profile["input_price_milli_usd_per_million"],
            cached_input_price_milli_usd_per_million=
                provider_profile.get("cached_input_price_milli_usd_per_million"),
            output_price_milli_usd_per_million=
                provider_profile["output_price_milli_usd_per_million"],
            api_safe_tool_names=provider_profile["api_safe_tool_names"],
            prefer_max_completion_tokens=
                provider_profile["prefer_max_completion_tokens"],
            omit_stream_options=provider_profile["omit_stream_options"],
            min_request_interval_ms=provider_profile["min_request_interval_ms"],
            upstream_timeout_secs=provider_profile.get("relay_upstream_timeout_secs", 120),
            repair_missing_tool_call_ids=
                provider_profile.get("repair_missing_tool_call_ids", False),
            api_protocol=api_protocol,
            allow_unauthenticated_loopback=(
                provider_profile.get("auth_mode") == "unauthenticated_loopback"
            ),
            max_retryable_408_retries=provider_profile.get(
                "max_retryable_408_retries", 0,
            ),
            standard_cyber_safeguards=args.standard_cyber_safeguards,
            single_file_edit_patch=args.single_file_edit_patches,
        )
        env = _clean_environment(repo, relay, model_id, api_protocol)
        result: dict = {"arm": arm, "harbor_exit": None, "runner_error": None,
                        "tool_attempts_observed": 0, "trials": []}
        try:
            _relay_smoke(relay, env)
            print(f"{arm}: relay smoke passed; starting {args.rounds}-decision bounded window", flush=True)
            job_root = cache / "jobs" / name
            code, reason, tool_attempts = run_harbor_bounded(
                harbor, config_path, repo, env, report_dir / f"{arm}-harbor.log",
                job_root, args.timeout_secs, args.tool_attempt_cap,
            )
            result["harbor_exit"] = code
            result["runner_error"] = reason
            result["tool_attempts_observed"] = tool_attempts
            result["trials"] = [
                summarize_trial(path.parent)
                for path in job_root.glob("live-database-cutover__*/result.json")
            ] if job_root.is_dir() else []
        except Exception as error:
            # Keep the category only; exception text may reflect provider data.
            result["runner_error"] = type(error).__name__
        finally:
            relay.close()
            relay.save_receipt(report_dir / f"{arm}-relay.json")
            result["provider_usage_settled"] = relay_usage_settled(relay)
            result["comparison_status"] = comparison_status(
                result, provider_usage_settled=result["provider_usage_settled"],
            )
            all_results.append(result)
            (report_dir / "results.json").write_text(
                json.dumps(all_results, indent=2), encoding="utf-8"
            )
            print(json.dumps(result), flush=True)
        if result["comparison_status"] != "COMPARABLE":
            print("bounded pair stopped after a non-comparable arm", flush=True)
            break
    print(f"bounded trial finished; evidence: {report_dir}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

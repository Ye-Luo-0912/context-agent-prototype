"""Zero-supplier preflight for the Terminal-Bench pilot.

This command is intentionally fail-closed.  It validates the locked task
selection and local CLI shape, then reports missing container/Harbor/runtime
capabilities as ``NOT_READY``.  It never starts a model, contacts a provider,
or changes a task directory.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
PILOT = ROOT / "docs" / "experiments" / "terminal-bench-pilot"
LOCK = PILOT / "selection.lock.json"
PRIMARY_IDS = [
    "wal-recovery-ordering",
    "mvcc-lsm-compaction",
    "session-window-debug",
    "payments-pipeline-fix",
    "rs-archive-clone",
    "live-database-cutover",
]
RESERVE_IDS = ["risk-scorer-replay", "distributed-dedup"]


def _sha256(path: Path) -> str | None:
    if not path.is_file():
        return None
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _probe_command(name: str, *args: str) -> dict[str, Any]:
    path = shutil.which(name)
    result: dict[str, Any] = {"command": name, "available": path is not None}
    if path is None:
        return result
    result["path"] = path
    if not args:
        return result
    try:
        completed = subprocess.run(
            [path, *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        result["probe_error"] = type(error).__name__
        return result
    result["exit_code"] = completed.returncode
    result["probe_ok"] = completed.returncode == 0
    # Version output is useful for a lock, but cap and normalize it.  Never
    # include the process environment or command arguments containing secrets.
    result["stdout_prefix"] = (completed.stdout or "")[:400].strip()
    result["stderr_prefix"] = (completed.stderr or "")[:400].strip()
    return result


def validate_lock(lock: dict[str, Any]) -> list[str]:
    errors: list[str] = []
    if lock.get("status") not in {
        "PLAN_ONLY_NOT_STARTED",
        "TB2_PRECHECK_BLOCKED",
        "TB2_CALIBRATED_READY_FOR_TB3",
    }:
        errors.append("selection lock has an unexpected execution state")
    if lock.get("upstream", {}).get("commit") != "452bf305c6daa62fc59061d22133a7cbc7c1572e":
        errors.append("selection lock does not point at Terminal-Bench v4.0.0 commit")
    tasks = lock.get("tasks")
    if not isinstance(tasks, list):
        return ["selection lock tasks is not a list"]
    ids = [item.get("id") for item in tasks]
    if ids != PRIMARY_IDS + RESERVE_IDS:
        errors.append(f"task order differs from locked selection: {ids!r}")
    primary = [item for item in tasks if item.get("role") == "primary"]
    if [item.get("id") for item in primary] != PRIMARY_IDS:
        errors.append("primary task set/order is incomplete")
    if len(primary) != 6:
        errors.append("expected six primary tasks")
    for item in primary:
        if item.get("official_agent_timeout_sec") != 28800.0:
            errors.append(f"{item.get('id')}: official agent timeout is not 8 hours")
        if item.get("verifier_environment_mode") != "separate":
            errors.append(f"{item.get('id')}: verifier is not separate")
        limits = item.get("per_trial_limits", {})
        if limits.get("maintenance_calls") != 0:
            errors.append(f"{item.get('id')}: maintenance calls are not zero")
        for field in ("agent_wall_seconds", "main_decisions", "provider_attempts", "input_tokens", "output_tokens"):
            if not isinstance(limits.get(field), int) or limits[field] <= 0:
                errors.append(f"{item.get('id')}: invalid positive limit {field}")
    stage_caps = lock.get("stage_provider_usd_caps", {})
    for stage, cap in stage_caps.items():
        amount = sum(
            item["per_trial_limits"]["estimated_provider_usd"] * 2
            for item in primary
            if item.get("stage") == stage
        )
        if amount != cap:
            errors.append(f"stage {stage} budget mismatch: {amount} != {cap}")
    return errors


def _find_binary() -> tuple[Path | None, list[dict[str, Any]]]:
    candidates: list[Path] = []
    configured = os.environ.get("CONTEXT_AGENT_TUI_BINARY")
    if configured:
        candidates.append(Path(configured))
    candidates.extend(
        [
            ROOT / "target-debian12" / "x86_64-unknown-linux-gnu" / "debug" / "agent-tui",
            ROOT / "target" / "x86_64-unknown-linux-gnu" / "debug" / "agent-tui",
            ROOT / "target" / "debug" / "agent-tui",
            ROOT / "target" / "debug" / "agent-tui.exe",
        ]
    )
    seen: set[Path] = set()
    observations: list[dict[str, Any]] = []
    for candidate in candidates:
        candidate = candidate.resolve() if candidate.exists() else candidate
        if candidate in seen:
            continue
        seen.add(candidate)
        item = {
            "path": str(candidate),
            "exists": candidate.is_file(),
            "executable": os.access(candidate, os.X_OK) if candidate.is_file() else False,
            "host_compatible": (
                (os.name == "nt" and candidate.suffix.lower() == ".exe")
                or (os.name != "nt" and candidate.suffix.lower() != ".exe")
            ),
            "sha256": _sha256(candidate),
        }
        observations.append(item)
        if item["exists"] and item["executable"] and item["host_compatible"]:
            return candidate, observations
    return None, observations


def _probe_cli(binary: Path | None) -> dict[str, Any]:
    if binary is None:
        return {"status": "NOT_RUN", "reason": "no executable agent-tui candidate"}
    try:
        completed = subprocess.run(
            [str(binary), "--help"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"status": "FAIL", "reason": type(error).__name__}
    stdout = completed.stdout or ""
    stderr = completed.stderr or ""
    output = stdout + "\n" + stderr
    flags = ["--context=", "--max-rounds=", "--timeout-secs=", "--grant-file=", "--jsonl-out=", "--prompt=", "--state-dir="]
    return {
        "status": "PASS" if completed.returncode == 0 and all(flag in output for flag in flags) else "FAIL",
        "exit_code": completed.returncode,
        "flags": {flag: flag in output for flag in flags},
        "stdout_prefix": stdout[:600],
        "stderr_prefix": stderr[:600],
    }


def run_preflight() -> dict[str, Any]:
    report: dict[str, Any] = {
        "schema": "context-agent.terminal-bench-pilot.preflight.v1",
        "status": "NOT_READY",
        "supplier_calls": 0,
        "provider_contacted": False,
        "root": str(ROOT),
        "platform": {"system": platform.system(), "machine": platform.machine(), "python": sys.version.split()[0]},
        "checks": {},
    }
    try:
        lock = json.loads(LOCK.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        report["checks"]["selection_lock"] = {"status": "FAIL", "reason": type(error).__name__}
        return report
    lock_errors = validate_lock(lock)
    report["checks"]["selection_lock"] = {
        "status": "PASS" if not lock_errors else "FAIL",
        "errors": lock_errors,
        "sha256": _sha256(LOCK),
    }
    harbor_bin = os.environ.get("HARBOR_BIN", "harbor")
    harbor_python = os.environ.get("HARBOR_PYTHON", sys.executable)
    runner = {
        "docker": _probe_command("docker", "version", "--format", "{{.Server.Version}}"),
        "podman": _probe_command("podman", "version", "--format", "{{.Version}}"),
        "harbor": _probe_command(harbor_bin, "--version"),
    }
    python_harbor = _probe_command(
        harbor_python, "-c", "import harbor; print('harbor-import-ok')"
    )
    report["checks"]["runner_commands"] = runner
    report["checks"]["harbor_python"] = python_harbor
    binary, candidates = _find_binary()
    report["checks"]["agent_binary"] = {
        "status": "PASS" if binary else "NOT_READY",
        "selected": str(binary) if binary else None,
        "candidates": candidates,
        "linux_compatible_candidate": bool(
            binary and platform.system() != "Windows" and not str(binary).lower().endswith(".exe")
        ),
    }
    report["checks"]["agent_cli"] = _probe_cli(binary)
    args_text = (ROOT / "crates" / "agent-tui" / "src" / "args.rs").read_text(encoding="utf-8")
    has_state_flag = "--state-dir" in args_text
    report["checks"]["artifact_state_isolation"] = {
        "status": "DECLARED" if has_state_flag else "BLOCKED",
        "runtime_exposes_state_dir": has_state_flag,
        "reason": None if has_state_flag else "agent-tui stores .focus-agent under the task root; official artifacts may include it",
    }
    report["checks"]["working_tree"] = {
        "status": "INFO",
        "note": "preflight does not mutate or clean the checkout",
    }
    # Harbor is the orchestration CLI; it still needs a local container
    # runtime unless a separate cloud environment is explicitly selected.
    has_runner = runner["docker"].get("probe_ok", False) or runner["podman"].get("probe_ok", False)
    has_harbor = runner["harbor"].get("probe_ok", False) or python_harbor.get("probe_ok", False)
    cli_ok = report["checks"]["agent_cli"].get("status") == "PASS"
    safe_state = has_state_flag
    linux_binary = report["checks"]["agent_binary"].get("linux_compatible_candidate", False)
    if not lock_errors and has_runner and has_harbor and cli_ok and safe_state and linux_binary:
        report["status"] = "STATIC_READY"
    else:
        blockers = []
        if lock_errors:
            blockers.append("selection_lock")
        if not has_runner:
            blockers.append("container_runtime")
        if not has_harbor:
            blockers.append("harbor")
        if not cli_ok or not linux_binary:
            blockers.append("linux_agent_binary")
        if not safe_state:
            blockers.append("artifact_state_isolation")
        report["blockers"] = blockers
    report["functional_smoke"] = "NOT_RUN"
    report["trial_ready"] = False
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="write the JSON report to this path")
    args = parser.parse_args(argv)
    report = run_preflight()
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded, encoding="utf-8")
    print(encoded, end="")
    # STATIC_READY proves only that local declarations and toolchain probes
    # are coherent. A trial may start only after the functional smoke,
    # provider/price/authorization freeze and artifact checks set trial_ready.
    return 0 if report["trial_ready"] else 2


if __name__ == "__main__":
    raise SystemExit(main())

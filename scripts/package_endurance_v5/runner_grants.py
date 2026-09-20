"""Authorization sized to the frozen campaign (review F09).

The shared runner's defaults are short-test defaults: 48 Python executions and a
30-minute expiry. A frozen V5 campaign asks for hundreds of model decisions,
continuous repair work and a two-hour candidate load, so those defaults are
structurally incompatible with the frozen action. This module derives the grant
set from the frozen campaign and refuses an incompatible pairing before any
window opens.

Renewal stays inside the original campaign budget: the expiry is the campaign
deadline, and the Python run allowance is a function of the frozen decision
budget, not an unbounded allowance.
"""
from __future__ import annotations

import json
import time
from pathlib import Path

PYTHON_RUNS_PER_DECISION = 8
PYTHON_RUNS_BASE = 64
SEGMENT_TAIL_SECONDS = 600
WRITE_CONTENT_BYTES = 160000
# The expiry check compares a wall-clock remainder with a frozen requirement;
# one second of tolerance keeps float arithmetic from refusing an exact fit.
EXPIRY_TOLERANCE_SECONDS = 1.0


def required_actions(caps: dict) -> dict:
    return dict(
        main_decisions=caps["main_decisions"],
        python_runs=caps["main_decisions"] * PYTHON_RUNS_PER_DECISION + PYTHON_RUNS_BASE,
        wall_clock_seconds=caps["target_load_seconds"] + SEGMENT_TAIL_SECONDS,
        declaration=(
            "one durable task that runs public and focused tests between repairs, "
            "kept alive for the frozen candidate load"
        ),
    )


def campaign_grants(caps: dict, *, python: str, now: float | None = None) -> list:
    """Grants that are bounded by the campaign, and large enough for it."""
    now = time.time() if now is None else now
    required = required_actions(caps)
    expires_at_ms = int(min(caps["deadline_epoch"],
                            max(caps["created_epoch"] + required["wall_clock_seconds"],
                                now + required["wall_clock_seconds"])) * 1000)
    return [
        {
            "id": "app-write",
            "risk": "WorkspaceWrite",
            "target": {"workspace_path_prefix": "app"},
            "constraint": {"max_content_bytes": WRITE_CONTENT_BYTES},
            "expires_at_ms": expires_at_ms,
        },
        {
            "id": "python-tests",
            "risk": "ProcessExecution",
            "target": {"exec_argv_prefix": [python]},
            "constraint": {"max_runs": required["python_runs"]},
            "expires_at_ms": expires_at_ms,
        },
    ]


def _grant(grants: list, grant_id: str) -> dict | None:
    return next((row for row in grants if row.get("id") == grant_id), None)


def compatibility(caps: dict, grants: list, *, now: float | None = None) -> dict:
    """Check the frozen action against the grants before a window opens."""
    now = time.time() if now is None else now
    required = required_actions(caps)
    problems = []
    write = _grant(grants, "app-write")
    python = _grant(grants, "python-tests")
    if write is None:
        problems.append("no workspace write grant")
    else:
        bound = (write.get("constraint") or {}).get("max_content_bytes")
        if not isinstance(bound, int) or bound < WRITE_CONTENT_BYTES:
            problems.append(f"workspace write grant too small: {bound!r}")
        if not write.get("target", {}).get("workspace_path_prefix"):
            problems.append("workspace write grant is not bound to a workspace path")
    if python is None:
        problems.append("no process execution grant")
    else:
        bound = (python.get("constraint") or {}).get("max_runs")
        if not isinstance(bound, int) or bound < required["python_runs"]:
            problems.append(
                f"process execution grant {bound!r} is below the required "
                f"{required['python_runs']} python runs")
        argv = python.get("target", {}).get("exec_argv_prefix")
        if not argv:
            problems.append("process execution grant is not bound to an executable prefix")
    expiries = [row.get("expires_at_ms") for row in grants if isinstance(row.get("expires_at_ms"), int)]
    covering = min(expiries) if len(expiries) == len(grants) and expiries else None
    if covering is None:
        problems.append("grants do not carry a bounded expiry")
    else:
        allowed_seconds = covering / 1000 - now
        if allowed_seconds + EXPIRY_TOLERANCE_SECONDS < required["wall_clock_seconds"]:
            problems.append(
                f"grant expiry leaves {allowed_seconds:.0f}s, below the required "
                f"{required['wall_clock_seconds']}s")
    return dict(compatible=not problems, problems=problems, required=required,
                granted=dict(expires_at_ms=covering, max_runs=(
                    (_grant(grants, "python-tests") or {}).get("constraint") or {}).get("max_runs")),
                checked_epoch=now)


def load(path) -> list:
    return json.loads(Path(path).read_bytes())

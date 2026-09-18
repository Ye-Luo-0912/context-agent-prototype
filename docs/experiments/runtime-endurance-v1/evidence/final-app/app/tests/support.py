"""Shared helpers for the P3 process-level checks."""

from __future__ import annotations

import json
import os
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))


def fresh_dir(prefix: str) -> Path:
    base = Path(os.environ.get("PLATFORM_TEST_TMP", ROOT / ".platform-test"))
    base.mkdir(parents=True, exist_ok=True)
    child = base / f"{prefix}-{uuid.uuid4().hex[:12]}"
    child.mkdir(parents=True)
    return child


def public_plan() -> list:
    return json.loads((ROOT / "fixtures" / "plan.json").read_text(encoding="utf-8"))


def public_rows() -> list:
    text = (ROOT / "fixtures" / "input.jsonl").read_text(encoding="utf-8")
    return [json.loads(line) for line in text.splitlines() if line.strip()]

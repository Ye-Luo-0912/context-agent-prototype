"""Cross-segment campaign accounting derived from the campaign's own materials.

The caller never supplies the consumed budget: it is read back from the
ledger and the segment summaries, so a resume cannot be granted a fresh
allowance by asking for one (review F10).
"""
from __future__ import annotations

import json
from pathlib import Path


def limits(stage):
    value = json.loads((Path(stage) / "campaign.json").read_bytes())
    if value.get("kind") != "v5_online_backup_continuous":
        raise ValueError("wrong V5 campaign kind")
    if value["deadline_epoch"] != value["created_epoch"] + 6 * 3600:
        raise ValueError("campaign deadline was changed")
    return value


def segment_summaries(stage):
    rows = []
    for summary in sorted(Path(stage).glob("*/summary.json")):
        try:
            rows.append(dict(segment=summary.parent.name,
                             value=json.loads(summary.read_bytes())))
        except (OSError, json.JSONDecodeError):
            rows.append(dict(segment=summary.parent.name, value=None))
    return rows


def campaign_accounting(stage):
    """Decisions, tool calls and provider attempts consumed so far."""
    stage = Path(stage)
    caps = limits(stage)
    ledger_path = stage / "budget-ledger.json"
    ledger = json.loads(ledger_path.read_bytes()) if ledger_path.exists() else {"attempts": []}
    attempts = ledger.get("attempts", [])
    summaries = segment_summaries(stage)
    decisions = 0
    tools = 0
    incomplete = []
    for row in summaries:
        value = row["value"]
        if not isinstance(value, dict):
            incomplete.append(row["segment"])
            continue
        decisions += int(value.get("rounds", 0) or 0)
        tools += int(value.get("tool_calls", 0) or 0)
    source = "ledger+segment summaries"
    if not summaries and attempts:
        # No summary survived: derive what the ledger alone can prove.
        decisions = max(int(row.get("request", 0) or 0) for row in attempts)
        source = "ledger only (no segment summary survived)"
    return dict(source=source, main_decisions=decisions, tool_attempts=tools,
                provider_attempts=len([row for row in attempts if row.get("status") != "rejected_cap"]),
                incomplete_segments=incomplete,
                limits=dict(main_decisions=caps["main_decisions"],
                            tool_attempts=caps["tool_attempts"],
                            provider_attempts=caps["provider_attempts"]))


def remaining_allowance(stage):
    accounting = campaign_accounting(stage)
    limits_value = accounting["limits"]
    return dict(accounting=accounting,
                main_decisions=max(limits_value["main_decisions"] - accounting["main_decisions"], 0),
                tool_attempts=max(limits_value["tool_attempts"] - accounting["tool_attempts"], 0),
                provider_attempts=max(limits_value["provider_attempts"] - accounting["provider_attempts"], 0))

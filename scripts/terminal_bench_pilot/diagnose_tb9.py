"""Read frozen TB9 receipts; emit only counts, hashes and event references.

Does not start Harbor, contact a provider, modify evidence or print tool bodies.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path


def diagnose(cache: Path, arm: str) -> dict:
    identity_path = cache / "tb9-bounded-b-live-20260923/identity.json"
    identity = json.loads(identity_path.read_text(encoding="utf-8"))
    grant_path = cache / "grants-live.json"
    grant_bytes = grant_path.read_bytes()
    grant_hash = hashlib.sha256(grant_bytes).hexdigest()
    if grant_hash != identity["grant_sha256"]:
        raise ValueError("current grant file does not match frozen TB9 identity")
    grants = json.loads(grant_bytes)
    python_grant = next(g for g in grants if g["target"].get("exec_argv_prefix") == ["python3"])
    job = cache / "jobs" / f"tb9-{arm}-live-bounded-400-20260923"
    trials = list(job.glob("live-database-cutover__*/agent/context-agent/events.jsonl"))
    if len(trials) != 1:
        raise ValueError("expected exactly one frozen trace per arm")
    trace = trials[0]
    calls, outputs, usages, context_samples = {}, [], [], []
    model_round = 0
    with trace.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            row = json.loads(line)
            event = row.get("event", {})
            if event.get("type") == "model_started":
                model_round += 1
            elif event.get("type") == "tool_started":
                call = event["call"]
                calls[call["id"]] = call
            elif event.get("type") == "tool_finished":
                output = event["output"]
                outputs.append({"round": model_round, "seq": row["seq"],
                                "line": line_number, "output": output,
                                "call": calls.get(output["call_id"])})
            elif event.get("type") == "model_used":
                usages.append((model_round, event))
            elif event.get("type") == "context_prepared":
                context_samples.append(event["diagnostics"])

    process = [r for r in outputs if r["output"]["tool_name"] == "process.run"]
    executed = [r for r in process if r["output"]["summary"].startswith("process completed (")]
    denied = [r for r in process if r["output"].get("metadata", {}).get("failure_class") == "approval_denied"]
    # Pre-dispatch schema/surface refusals need not emit ToolStarted. Actual
    # execution and approval-denied process calls must have their argv evidence.
    if any(r["call"] is None for r in executed + denied):
        raise ValueError("process execution/approval result has no matching argv evidence")
    python_denied = [r for r in denied if r["call"]["arguments"].get("argv", [])[:1] == ["python3"]]
    if len(executed) != python_grant["constraint"]["max_runs"]:
        raise ValueError("executed count does not match the hypothesized grant cap")
    if any(r["call"]["arguments"].get("argv", [])[:1] != ["python3"] for r in executed):
        raise ValueError("executed processes include another grant scope")
    last_round = executed[-1]["round"]
    post = [r for r in outputs if r["round"] > last_round]
    signatures = Counter(json.dumps(r["call"]["arguments"], sort_keys=True) for r in python_denied)
    return {
        "arm": arm, "trace": str(trace),
        "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
        "grant_sha256": grant_hash, "python_max_runs": python_grant["constraint"]["max_runs"],
        "model_rounds": model_round, "all_tool_results": len(outputs),
        "tool_results_without_started_call": sum(r["call"] is None for r in outputs),
        "executed_processes": len(executed),
        "executed_process_nonzero": sum(not r["output"]["ok"] for r in executed),
        "last_executed": {k: executed[-1][k] for k in ("round", "seq", "line")},
        "first_python_denied": {k: python_denied[0][k] for k in ("round", "seq", "line")},
        "process_denials_total": len(denied), "python_denials": len(python_denied),
        "process_denials_after_exhaustion": sum(r["round"] > last_round for r in denied),
        "same_python_arguments_max_denials": max(signatures.values()),
        "post_exhaustion_rounds": model_round - last_round,
        "post_exhaustion_input_tokens": sum(e["input_tokens"] for r, e in usages if r > last_round),
        "post_exhaustion_output_tokens": sum(e["output_tokens"] for r, e in usages if r > last_round),
        "post_exhaustion_successful_reads": sum(r["output"]["tool_name"] == "fs.read" and r["output"]["ok"] for r in post),
        "context_prepared_samples": len(context_samples),
        "context_ranges": {
            key: {"min": min(sample[key] for sample in context_samples),
                  "max": max(sample[key] for sample in context_samples)}
            for key in ("total_items", "tool_round", "gc_evicted_total", "resident_items")
        },
        "post_exhaustion_file_edits": [
            {"round": r["round"], "seq": r["seq"], "tool": r["output"]["tool_name"],
             "no_op": r["output"]["summary"].startswith("no-op:")}
            for r in post if r["output"]["ok"] and r["output"]["tool_name"] in ("fs.write", "edit.patch")
        ],
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cache", type=Path)
    args = parser.parse_args()
    print(json.dumps([diagnose(args.cache, arm) for arm in ("dynamic", "rolling")], indent=2))

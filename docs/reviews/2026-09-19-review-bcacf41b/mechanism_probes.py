#!/usr/bin/env python3
"""Offline mechanism checks for bcacf41b.

These checks DO NOT import/execute the Rust repository, call a provider, or
replay the user's campaign. Most are explicit control-flow models of the
cited functions. One check uses a disposable local Popen child and always
kills/reaps it. All file writes are in a fresh temporary directory.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

BASE = "bcacf41b9104db6ebada7adfc2a95de5e341f49b"


def capture_flag_probe() -> dict[str, Any]:
    # Models lifecycle::resume_failed_turn_checkpoint +
    # safepoint::safe_point_resume_commit. Both lanes are occupied; the
    # safe-point method's actual early return cannot capture the new debt.
    debt = {"FailedTurnYield"}
    old_prepare_exists = True
    gc_busy = True
    captured = False
    new_snapshot_created = False
    if gc_busy and not captured:
        if not old_prepare_exists:
            new_snapshot_created = True
            debt.clear()
        captured = True  # the reviewed caller sets this unconditionally
    # The old preparation's frozen debt excludes FailedTurnYield.
    old_prepare_exists = False
    gc_busy = False
    if not captured:
        new_snapshot_created = True
        debt.clear()
    fenced = bool(debt)
    assert captured and not new_snapshot_created and fenced
    return {
        "kind": "CONTROL_FLOW_MODEL_NOT_RUST_REPRO",
        "source": ["actor/lifecycle.rs::resume_failed_turn_checkpoint", "actor/safepoint.rs::safe_point_resume_commit"],
        "precondition": "old checkpoint prepare and GC lane both occupied",
        "captured_flag": captured,
        "new_snapshot_created": new_snapshot_created,
        "remaining_debt": sorted(debt),
        "continuation_fenced": fenced,
        "production_interleaving_executed": False,
    }


def semantic_intent_probe() -> dict[str, Any]:
    # Matching itself is a precondition: two old entries satisfy the SAME
    # existing predicate. Models disposal after any match in one batch.
    states = {"old-A": "Live", "old-B": "Live"}
    pending = ["withdrawal"]
    for batch in [["old-A"], ["old-B"]]:
        retained = []
        for intent in pending:
            consumed = False
            for entry in batch:
                states[entry] = "Superseded"
                consumed = True
            if not consumed:
                retained.append(intent)
        pending = retained
    assert states == {"old-A": "Superseded", "old-B": "Live"}
    # An unmatched retained predicate contains no creation/generation bound.
    intent_created = 10
    new_entry_created = 20
    predicate_matches = True  # same task/entity/kind; not the by_id
    applies_to_newer = predicate_matches  # reviewed code tests no causal cutoff
    assert new_entry_created > intent_created and applies_to_newer
    # The deferred path copies only 4000 chars before its negation test.
    text = "Remove AuthService.rs " + "filler " * 610 + " Do not remove AuthService.rs."
    assert len(text) > 4000
    def protected(s: str) -> bool:
        return "not" in s.lower().split()
    full_protected = protected(text)
    copied_protected = protected(text[:4000])
    assert full_protected and not copied_protected
    return {
        "kind": "CONTROL_FLOW_AND_TRUNCATION_MODEL_NOT_RUST_REPRO",
        "source": "context-simple/gc/reachability.rs::apply_cold_semantic_intents_on_install",
        "two_batch_states": states,
        "remaining_intents": pending,
        "newer_matching_entry_eligible_without_cutoff": applies_to_newer,
        "message_chars": len(text),
        "full_text_negation_protected": full_protected,
        "copied_prefix_negation_protected": copied_protected,
    }


def coverage_probe() -> dict[str, Any]:
    # Cap 2 is a reduced parameter; this models the same saturation branch
    # at any real finite MAX_EVIDENCE_COVERAGE_WINDOWS, not its production value.
    cap = 2
    known_windows = [(1, 10), (21, 30)]
    incoming = (41, 50)
    known = any(a <= incoming[0] and b >= incoming[1] for a, b in known_windows)
    saturated = not known and len(known_windows) >= cap
    advanced = not known and not saturated
    equivalent = False or known or saturated
    result = "Repeated" if equivalent and not advanced else "Advanced"
    assert not known and result == "Repeated"
    return {"kind": "PARAMETERIZED_CONTROL_FLOW_MODEL", "source": "execution/state.rs::record_observation_evidence", "probe_cap_not_production_constant": cap, "known": known_windows, "incoming": incoming, "known_covered": known, "result": result, "drops_source_body": False}


def usage_from_sse(raw: bytes) -> dict[str, Any] | None:
    # Exact small parser logic transcribed from the reviewed runner.
    latest = None
    for line in raw.splitlines():
        if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
            continue
        try:
            event = json.loads(line[6:])
        except Exception:
            continue
        usage = (event.get("response") or {}).get("usage") or event.get("usage")
        if isinstance(usage, dict) and any(k in usage for k in ("input_tokens", "output_tokens", "cached_input_tokens")):
            latest = usage
    return latest


def budget_probe() -> dict[str, Any]:
    # These are illustrative numeric inputs, NOT actual account charges.
    cap = 1.00
    spent = 0.99
    admitted = spent < cap
    next_actual_estimate = 0.10
    after = spent + next_actual_estimate
    assert admitted and after > cap
    usage = usage_from_sse(b'data: {"usage":{"input_tokens":1000}}\n\n')
    assert usage is not None
    output = int(usage.get("output_tokens") or 0)
    cached = int(usage.get("cached_input_tokens") or 0)
    marked_unknown = usage is None
    assert output == 0 and cached == 0 and not marked_unknown
    segments = [0.60, 0.60]
    each_segment_passes = all(x < cap for x in segments)
    assert each_segment_passes and sum(segments) > cap
    return {"kind": "RUNNER_LOGIC_PROBE_SYNTHETIC_AMOUNTS", "source": "scripts/runtime_endurance_incremental_runner.py", "new_request_admitted": admitted, "illustrative_limit": cap, "illustrative_spend_after_response": round(after, 4), "partial_usage": usage, "missing_output_converted_to": output, "missing_cache_converted_to": cached, "partial_usage_marked_unknown": marked_unknown, "independent_segments_each_below_limit": each_segment_passes, "illustrative_combined_spend": sum(segments), "provider_calls": 0, "actual_user_budget_exceeded": "NOT_DETERMINED"}


def timeout_probe() -> dict[str, Any]:
    # Run only our disposable local child. The reviewed runner does not
    # perform this finally cleanup; this PROBE does, to leave no residue.
    proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(10)"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    raised = False
    alive = False
    try:
        try:
            proc.communicate(timeout=0.08)
        except subprocess.TimeoutExpired:
            raised = True
            alive = proc.poll() is None
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.communicate(timeout=5)
    assert raised and alive and proc.returncode is not None
    return {"kind": "ACTUAL_LOCAL_POPEN_CHECK_NOT_RUNTIME_TEST", "timeout_raised": raised, "child_alive_immediately_after_communicate_timeout": alive, "probe_cleanup_reaped_child": proc.returncode is not None, "remaining_probe_child": False, "tests_windows_tree_cleanup": False}


def overwrite_probe() -> dict[str, Any]:
    # Same exist_ok/write_text semantics as setup, in a temporary directory.
    with tempfile.TemporaryDirectory(prefix="endurance-setup-check-") as d:
        root = Path(d)
        app = root / "workspace" / "app"
        app.mkdir(parents=True, exist_ok=True)
        tracked = app / "engine.py"
        extra = app / "new_module.py"
        receipt = root / "baseline-lock.json"
        tracked.write_text("accepted implementation\n", encoding="utf-8")
        extra.write_text("model-added implementation\n", encoding="utf-8")
        receipt.write_text('{"status":"COMPLETE"}', encoding="utf-8")
        before = hashlib.sha256(tracked.read_bytes()).hexdigest()
        app.mkdir(parents=True, exist_ok=True)
        tracked.write_text("v1 seed\n", encoding="utf-8")
        receipt.write_text('{"status":"PREPARED","provider_attempts":0}', encoding="utf-8")
        result = {"kind": "ACTUAL_TEMPORARY_FILE_POLICY_CHECK_NOT_USER_CAMPAIGN", "previous_implementation_overwritten": hashlib.sha256(tracked.read_bytes()).hexdigest() != before, "additional_module_left_behind": extra.exists(), "baseline_replaced": json.loads(receipt.read_text())["status"] == "PREPARED"}
        assert all(result[k] for k in ("previous_implementation_overwritten", "additional_module_left_behind", "baseline_replaced"))
        return result


def finalization_probe() -> dict[str, Any]:
    completion_repair_terminal = False
    budget_finalization = True
    unavailable_must = ["unavailable.verifier"]
    rejects = not completion_repair_terminal and bool(unavailable_must)
    assert budget_finalization and rejects
    return {"kind": "CONTROL_FLOW_MODEL", "source": "actor/model.rs budget finalization vs unavailable_must", "text_only_finalization_selected": budget_finalization, "model_still_refused": rejects, "authorization_relaxed": False}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=Path(__file__).with_name("MECHANISM_RESULTS.json"))
    args = parser.parse_args()
    report = {"schema": "review-mechanisms/v1", "baseline": BASE, "python": sys.version.split()[0], "platform": platform.system(), "scope": "offline control-flow models and two disposable OS/file API checks; no Rust/.NET/paid-model execution", "provider_calls": 0, "probes": {"checkpoint_capture_flag": capture_flag_probe(), "cold_semantic_intents": semantic_intent_probe(), "coverage_saturation": coverage_probe(), "relay_budget_and_partial_usage": budget_probe(), "popen_timeout": timeout_probe(), "setup_overwrite": overwrite_probe(), "budget_finalization": finalization_probe()}}
    args.out.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(json.dumps({"checks_completed": len(report["probes"]), "provider_calls": 0, "output": str(args.out)}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

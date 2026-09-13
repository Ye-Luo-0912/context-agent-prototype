"""COST-5 paired acceptance runner and accounting extractor.

Two modes:

--from-evidence DIR
    Dry-run: recompute the COST-5 accounting table from existing run
    evidence (segment directories carrying events.jsonl, produced by the
    2026-09-11 flash-workflow harness or by --paired runs). Makes no
    network calls. Fields a pre-CORE-4/COST-2 run does not carry
    (usage_identity, cache write/miss, role) are reported as
    "unreported" — never as observed zeros (COST-6/COST-7 semantics).

--paired --out DIR
    Real run: executes the fixed three-task walkthrough (a/b/c, same
    seeds, grants, and product verification as the 2026-09-11 harness,
    imported from that run.py so the source tree stays single-sourced)
    in two same-start arms —
      arm "default":  compose defaults (4 calls / unbounded tokens)
      arm "budgeted": MAINTENANCE_MAX_CALLS_PER_MAINTAIN /
                      MAINTENANCE_MAX_TOKENS_PER_MAINTAIN /
                      MAINTENANCE_COMPACT_FAILURE_BACKOFF set
    and writes one paired accounting table (JSON + Markdown) with the
    quality gates. Credentials are scrubbed from every captured byte.

Token arithmetic is the ONLY thing this script may conclude on its own.
A "cost reduction" claim requires the full COST-5 gate (same quality,
non-overlapping billing buckets, recovery/resource non-regression) and
stays a human judgement recorded in the receipt, not a script verdict.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import sys
from pathlib import Path
from statistics import median

HERE = Path(__file__).resolve().parent
FLASH_RUN = HERE.parents[2] / "docs/reviews/2026-09-11-flash-workflow/run.py"


def load_flash_harness():
    spec = importlib.util.spec_from_file_location("flash_run", FLASH_RUN)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _rows(events_path: Path):
    for line in events_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except ValueError:
            continue
        yield row


def _u64(event: dict, name: str):
    value = event.get(name)
    return value if isinstance(value, (int, float)) else None


def _identity(event: dict) -> str:
    value = event.get("usage_identity")
    return value if isinstance(value, str) else "unreported"


def _role(event: dict) -> str:
    value = event.get("role")
    return value if isinstance(value, str) else "unreported"


def _bucket(event: dict, name: str):
    """COST-6: an absent cache counter stays absent (None), a present one
    is kept verbatim — no flattening, no derived fills."""
    if name in event:
        value = event.get(name)
        return value if isinstance(value, (int, float)) else "invalid"
    usage = event.get("usage")
    if isinstance(usage, dict) and name in usage:
        value = usage.get(name)
        return value if isinstance(value, (int, float)) else "invalid"
    return None


def segment_table(segment: str, events_path: Path, result_path: Path | None) -> dict:
    events = [row.get("event", {}) for row in _rows(events_path) if isinstance(row.get("event"), dict)]
    counts = {}
    for event in events:
        kind = event.get("type")
        if isinstance(kind, str):
            counts[kind] = counts.get(kind, 0) + 1

    def used_rows():
        return [e for e in events if e.get("type") == "model_used"]

    main_rows = [e for e in used_rows() if _role(e) in ("main", "unreported")]
    maint_rows = [e for e in used_rows() if _role(e) == "maintenance"]

    def lane_table(rows):
        observed = [e for e in rows if _identity(e) == "observed"]
        table = {
            "rows": len(rows),
            "identity": {name: sum(1 for e in rows if _identity(e) == name)
                         for name in ("observed", "estimated", "unknown", "unreported")},
            "input_tokens": sum(_u64(e, "input_tokens") or 0 for e in observed),
            "output_tokens": sum(_u64(e, "output_tokens") or 0 for e in observed),
            "cache_read_input_tokens": sum(_bucket(e, "cached_input_tokens") or 0 for e in observed),
            "attempts": sum(_u64(e, "attempts") or 0 for e in rows),
            "retries": sum(_u64(e, "retries") or 0 for e in rows),
            # Raw provider counters over ALL rows regardless of identity:
            # evidence the provider did report stays a lower bound even when
            # the row predates the typed identity (COST-7: known values are
            # never dropped, and never promoted into the observed bill).
            "raw_input_tokens_lower_bound": sum(_u64(e, "input_tokens") or 0 for e in rows),
            "raw_output_tokens_lower_bound": sum(_u64(e, "output_tokens") or 0 for e in rows),
            "raw_cached_input_tokens_lower_bound": sum(
                (_bucket(e, "cached_input_tokens") or 0) for e in rows
                if isinstance(_bucket(e, "cached_input_tokens"), (int, float))),
        }
        # COST-6: write and miss are independent provider observations —
        # summed only over rows where the provider actually reported them.
        for name in ("cache_write_input_tokens", "cache_miss_input_tokens"):
            reported = [(_bucket(e, name) or 0) for e in rows
                        if isinstance(_bucket(e, name), (int, float))]
            table[name] = sum(reported)
            table[name + "_reported_rows"] = len(reported)
        return table

    compactions = []
    gc_reports = []
    for event in events:
        if event.get("type") == "context_maintained":
            report = event.get("report") or {}
            for row in report.get("compactions") or []:
                compactions.append({
                    "reason": row.get("reason"),
                    "identity": row.get("usage_identity", "unreported"),
                    "input_tokens": _u64(row, "input_tokens") or 0,
                    "output_tokens": _u64(row, "output_tokens") or 0,
                    "cache_read_input_tokens": row.get("cached_input_tokens")
                        if isinstance(row.get("cached_input_tokens"), (int, float)) else None,
                    "cache_write_input_tokens": row.get("cache_write_input_tokens")
                        if isinstance(row.get("cache_write_input_tokens"), (int, float)) else None,
                    "cache_miss_input_tokens": row.get("cache_miss_input_tokens")
                        if isinstance(row.get("cache_miss_input_tokens"), (int, float)) else None,
                    "retries": _u64(row, "retries") or 0,
                })
        elif event.get("type") == "context_gc":
            report = event.get("report") or {}
            gc_reports.append({
                "evicted": _u64(report, "evicted") or 0,
                "externalized": _u64(report, "externalized") or 0,
                "store_write_bytes": _u64(report, "store_write_bytes") or 0,
                "store_io_failures": _u64(report, "store_io_failures") or 0,
            })

    result = {}
    if result_path and result_path.exists():
        try:
            result = json.loads(result_path.read_text(encoding="utf-8"))
        except ValueError:
            result = {}

    walls = [_u64(result, "wall_ms")] if isinstance(result, dict) else []
    lower_bound = any(
        _identity(e) in ("estimated", "unknown", "unreported") or (_u64(e, "retries") or 0) > 0
        for e in used_rows()
    ) or any(row["identity"] in ("estimated", "unknown", "unreported") or row["retries"] > 0
             for row in compactions)

    return {
        "segment": segment,
        "quality": {
            "exit_code": result.get("exit_code") if isinstance(result, dict) else None,
            "forced_tree_stop": result.get("forced_tree_stop") if isinstance(result, dict) else None,
            "wall_ms": result.get("wall_ms") if isinstance(result, dict) else None,
            "event_counts": counts,
        },
        "decision_lane": lane_table(main_rows),
        "maintenance_lane": lane_table(maint_rows),
        "compaction_rows": {
            "count": len(compactions),
            "identity": {name: sum(1 for r in compactions if r["identity"] == name)
                         for name in ("observed", "estimated", "unknown", "unreported")},
            "input_tokens": sum(r["input_tokens"] for r in compactions),
            "output_tokens": sum(r["output_tokens"] for r in compactions),
            "cache_read_input_tokens": sum(r["cache_read_input_tokens"] or 0 for r in compactions),
            "cache_write_input_tokens": sum(r["cache_write_input_tokens"] or 0 for r in compactions),
            "cache_miss_input_tokens": sum(r["cache_miss_input_tokens"] or 0 for r in compactions),
            "retries": sum(r["retries"] for r in compactions),
        },
        "gc": {
            "passes": len(gc_reports),
            "evicted": sum(r["evicted"] for r in gc_reports),
            "externalized": sum(r["externalized"] for r in gc_reports),
            "store_write_bytes": sum(r["store_write_bytes"] for r in gc_reports),
            "store_io_failures": sum(r["store_io_failures"] for r in gc_reports),
        },
        "bill_lower_bound": lower_bound,
        "_walls_ms": walls,
    }


def from_evidence(root: Path) -> int:
    tables = []
    for events_path in sorted(root.glob("*/**/events.jsonl")):
        segment = events_path.parent.name
        result_path = events_path.parent / "result.json"
        tables.append(segment_table(segment, events_path, result_path))
    if not tables:
        print(json.dumps({"error": f"no events.jsonl under {root}"}))
        return 1
    walls = sorted(t["quality"]["wall_ms"] for t in tables
                   if isinstance(t.get("quality", {}).get("wall_ms"), (int, float)))
    summary = {
        "segments": tables,
        "wall_ms_p50": median(walls) if walls else None,
        "note": "pre-CORE-4/COST-2 evidence reports identity=unreported and no cache split; "
                "absent counters are recorded as unreported, never as observed zeros",
    }
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    out = root / "cost5_accounting.json"
    out.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"written: {out}", file=sys.stderr)
    return 0


def paired(out_dir: Path) -> int:
    """Two same-start arms through the ORIGINAL fixed walkthrough main().

    The original flow owns the environment (official DeepSeek endpoint,
    responses protocol, provider_default cache, reasoning off), the fixed
    per-case segment choreography (including case c's cancel/restore), the
    source/binary-bound manifest, and per-task product verification. The
    arms differ ONLY in the COST-8 product knobs (maintenance budget env),
    injected through the process environment so the child agent-tui picks
    them up at startup.
    """
    flash = load_flash_harness()
    arms = {
        "default": {},
        "budgeted": {
            "MAINTENANCE_MAX_CALLS_PER_MAINTAIN": "2",
            "MAINTENANCE_MAX_TOKENS_PER_MAINTAIN": "20000",
            "MAINTENANCE_COMPACT_FAILURE_BACKOFF": "4",
        },
    }
    key = os.environ.get("OPENAI_API_KEY", "")
    if not key:
        print(json.dumps({"error": "OPENAI_API_KEY missing; source eval.env first"}),
              file=sys.stderr)
        return 1
    out_dir.mkdir(parents=True, exist_ok=False)
    runs = {}
    for arm, overrides in arms.items():
        # The original main() reads the credential from DEEPSEEK_API_KEY and
        # builds its own fixed provider environment; COST-8 knobs flow
        # through the process environment to the child binary.
        os.environ["DEEPSEEK_API_KEY"] = key
        saved = {name: os.environ.get(name) for name in overrides}
        os.environ.update(overrides)
        sys.argv = ["flash-run.py"]
        try:
            flash.main()
        finally:
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        latest = json.loads(
            (FLASH_RUN.parent / "latest.json").read_text(encoding="utf-8"))
        runs[arm] = {"run_dir": latest["run_dir"],
                     "verified": latest.get("case_results"),
                     "sources_unchanged": latest.get("sources_unchanged"),
                     "budget_overrides": overrides}
        print(json.dumps({"arm": arm, **runs[arm]}), flush=True)

    # Accounting table per arm from its own run evidence.
    arm_tables = {}
    for arm, info in runs.items():
        evidence = Path(info["run_dir"])
        tables = []
        for events_path in sorted(evidence.glob("*/**/events.jsonl")):
            tables.append(segment_table(events_path.parent.name, events_path,
                                        events_path.parent / "result.json"))
        arm_tables[arm] = tables

    summary = {
        "arms": runs,
        "accounting": arm_tables,
        "gate": "cost-reduction claims need the receipt-level human review: same quality "
                "on both arms (verification verdicts), non-overlapping buckets, honest "
                "lower_bound flags, recovery/resource non-regression",
    }
    (out_dir / "cost5_paired.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--from-evidence", type=Path, help="recompute accounting from evidence dir")
    parser.add_argument("--paired", action="store_true", help="execute the two-arm real run")
    parser.add_argument("--out", type=Path, help="output dir for --paired")
    args = parser.parse_args()
    if args.from_evidence:
        return from_evidence(args.from_evidence)
    if args.paired:
        if not args.out:
            print(json.dumps({"error": "--paired needs --out"}), file=sys.stderr)
            return 1
        return paired(args.out)
    parser.print_help()
    return 1


if __name__ == "__main__":
    raise SystemExit(main())

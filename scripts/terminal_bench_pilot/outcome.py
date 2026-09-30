"""Keep runtime termination, Harbor exception and official grading independent."""
import json
from pathlib import Path


def summarize_trial(path: Path) -> dict:
    result_path = path / "result.json"
    result = json.loads(result_path.read_text(encoding="utf-8")) if result_path.exists() else {}
    summary = {"trial": path.name,
               "harbor_exception": (result.get("exception_info") or {}).get("exception_type"),
               "rewards": (result.get("verifier_result") or {}).get("rewards"),
               "runtime": None, "verifier": None}
    events = path / "agent/context-agent/events.jsonl"
    if events.exists():
        last_surface_finalization = None
        finalization_model_used = False
        with events.open(encoding="utf-8") as stream:
            for line in stream:
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                event = row.get("event")
                if isinstance(event, dict) and event.get("type") == "tool_surface_planned":
                    report = event.get("report") or {}
                    omitted = report.get("omitted") or []
                    last_surface_finalization = (
                        "decision_budget_finalization"
                        if not report.get("selected") and any(
                            isinstance(item, dict)
                            and item.get("reason") == "decision_budget_finalization"
                            for item in omitted
                        ) else None
                    )
                    finalization_model_used = False
                elif (isinstance(event, dict) and event.get("type") == "model_used"
                      and last_surface_finalization is not None):
                    finalization_model_used = True
                if row.get("kind") == "session_end":
                    summary["runtime"] = {key: row.get(key) for key in (
                        "status", "exit", "approval_denied", "task_completed", "round_budget")}
                    summary["runtime"]["finalization_reason"] = (
                        last_surface_finalization
                        if row.get("status") == "completed" and finalization_model_used
                        else None
                    )
    ctrf = path / "verifier/ctrf.json"
    if ctrf.exists():
        report = json.loads(ctrf.read_text(encoding="utf-8"))
        summary["verifier"] = report.get("results", {}).get("summary")
    # Exit 3 records a denial somewhere in the turn; it does not prove that
    # the verifier was skipped or that every subsequent action was rejected.
    summary["grading_status"] = "GRADED" if summary["rewards"] is not None else "NO_GRADE"
    return summary


if __name__ == "__main__":
    import sys
    print(json.dumps(summarize_trial(Path(sys.argv[1])), indent=2))

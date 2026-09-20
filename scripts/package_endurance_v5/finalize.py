"""Freeze a read-only V5 execution receipt without changing the campaign."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]


def load(path):
    return json.loads(Path(path).read_bytes())


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main(stage, output):
    stage = Path(stage).resolve()
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    baseline = load(stage / "baseline-lock.json")
    work = stage / "workspace"
    current = {p.relative_to(work).as_posix(): digest(p) for p in work.rglob("*")
               if p.is_file() and "__pycache__" not in p.parts
               and "runtime-feedback" not in p.relative_to(work).parts}
    protected_unchanged = all(current.get(name) == value for name, value in baseline["files"].items())
    ledger = load(stage / "budget-ledger.json")
    first = load(stage / "model-continuous" / "summary.json")
    second = load(stage / "model-continuous-repair-01" / "summary.json")
    preflight = load(stage / "PREFLIGHT.json")
    feedback = load(work / "runtime-feedback/latest.json")
    events = work / "runtime-feedback/events.jsonl"
    event_count = sum(1 for _ in events.open(encoding="utf-8")) if events.exists() else 0
    task_ids = set()
    for event_file in (stage / "model-continuous/events.jsonl",
                       stage / "model-continuous-repair-01/events.jsonl"):
        if event_file.exists():
            for line in event_file.read_text(encoding="utf-8", errors="replace").splitlines():
                try:
                    task = json.loads(line).get("event", {}).get("task_id")
                except json.JSONDecodeError:
                    task = None
                if task:
                    task_ids.add(task)
    model_tests = (ROOT / "target/package-endurance-v5-20260921-r3/model-tests.log").read_text(
        encoding="utf-8", errors="replace")
    ran = re.search(r"Ran (\d+) tests", model_tests)
    failed = len(re.findall(r"^ERROR: ", model_tests, re.MULTILINE))
    root = stage / "continuous-load" / "repository"
    sys.path.insert(0, str(ROOT / "scripts/package_endurance_v5"))
    from oracle import repository_cut, summary
    descriptor, members = repository_cut(root)
    source_summary = summary(descriptor, members)
    candidate = work / "app/live_backup.py"
    status = dict(
        status="NOT_ACCEPTED_MODEL_BUDGET_EXHAUSTED",
        campaign=str(stage),
        head=baseline["head"],
        provider_paid_calls=len(ledger["attempts"]),
        main_decisions=first["rounds"] + second["rounds"],
        estimated_committed_usd=ledger["committed_usd"],
        reserved_usd=ledger["reserved_usd"],
        unknown_usd=ledger["unknown_usd"],
        cap_stopped=ledger["cap_stopped"],
        preflight=preflight,
        segments=[first, second],
        task_ids=sorted(task_ids),
        protected_files=len(baseline["files"]),
        protected_unchanged=protected_unchanged,
        candidate_sha256=digest(candidate),
        candidate_present=True,
        candidate_local_tests=dict(total=int(ran.group(1)) if ran else None,
                                   errors=failed, exit_code=1),
        continuous_load=dict(feedback=feedback, event_records=event_count,
                             source_summary=source_summary,
                             controller_outputs=[
                                 "continuous-load",
                                 "continuous-load-resume-01",
                                 "continuous-load-resume-02",
                                 "continuous-load-resume-03",
                             ]),
        independent_oracle=dict(calibration=preflight["oracle_calibration"],
                                final_source_summary=source_summary),
        limits=dict(main_decisions=260, provider_attempts=300, tool_attempts=900,
                    estimated_cost_usd=2.0, target_load_seconds=7200),
        remote_ci="NOT_RUN",
        submitted=False,
        notes=[
            "First model segment stopped at model_output_limit after 41 rounds; no candidate file existed.",
            "Same TaskId/workspace repair segment used 219 rounds and stopped with approval_denied after turn_completed.",
            "Candidate still refused live SQLite WAL sidecars and initially capped historical receipts.",
            "Continuous load was stopped after the model budget ended; it never reached a passing archive cycle.",
            "No manual edits were applied to the candidate workspace after model termination.",
        ],
    )
    (output / "FINAL_STATUS.json").write_text(json.dumps(status, ensure_ascii=False, indent=2) + "\n",
                                                 encoding="utf-8")
    manifest = {p.relative_to(output).as_posix(): digest(p) for p in output.rglob("*") if p.is_file()}
    (output / "MANIFEST.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
                                           encoding="utf-8")
    print(json.dumps(dict(status=status["status"], candidate_sha256=status["candidate_sha256"],
                          provider_paid_calls=status["provider_paid_calls"],
                          estimated_committed_usd=status["estimated_committed_usd"],
                          source_summary=source_summary, candidate_tests=status["candidate_local_tests"]),
               ensure_ascii=False))


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    main(args.stage, args.output)

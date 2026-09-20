"""Freeze a read-only V5 execution receipt derived from the campaign materials.

Nothing about the outcome is hardcoded. The status, the candidate identity, the
segment list, the controller outputs and the candidate test log are read back
from the campaign directory; a missing or unreadable material makes the receipt
``INCOMPLETE`` and lists what is missing instead of substituting a remembered
value (review F11).

The result generator therefore cannot turn a partial campaign into a success,
and a caller cannot point it at a different stage to reuse an old verdict.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

from oracle import repository_cut, summary  # noqa: E402

REQUIRED_MATERIALS = (
    "campaign.json",
    "baseline-lock.json",
    "contract-identity.json",
    "capacity-plan.json",
)


class MissingMaterial(RuntimeError):
    pass


def load(path):
    path = Path(path)
    if not path.is_file():
        raise MissingMaterial(f"missing material: {path}")
    try:
        return json.loads(path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise MissingMaterial(f"unreadable material: {path}: {error}") from error


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def segment_receipts(stage):
    """Every model segment is discovered, never assumed to be the fixed two."""
    rows = []
    for summary_path in sorted(Path(stage).glob("*/summary.json")):
        out_dir = summary_path.parent
        if out_dir.name.startswith("continuous-load"):
            continue
        value = load(summary_path)
        rows.append(dict(
            segment=out_dir.name,
            outcome=value.get("outcome"), exit_code=value.get("exit_code"),
            rounds=value.get("rounds"), tool_calls=value.get("tool_calls"),
            requests=value.get("requests"), elapsed=value.get("elapsed"),
            categories=value.get("categories"), protected_unchanged=value.get("protected_unchanged"),
            terminals=value.get("terminals"), budget=value.get("budget"),
            session=value.get("session"),
        ))
    return rows


def controller_receipts(stage):
    rows = []
    for receipt_path in sorted(Path(stage).glob("continuous-load*/receipt.json")):
        rows.append(load(receipt_path))
    return rows


def candidate_tests(stage):
    """The candidate's own test log is discovered; a missing log is reported."""
    logs = sorted(Path(stage).glob("**/model-tests.log"))
    if not logs:
        return dict(status="NOT_RECORDED", log=None)
    text = logs[0].read_text(encoding="utf-8", errors="replace")
    ran = re.search(r"Ran (\d+) tests", text)
    errors = len(re.findall(r"^ERROR: ", text, re.MULTILINE))
    failures = len(re.findall(r"^FAIL: ", text, re.MULTILINE))
    return dict(status="RECORDED", log=str(logs[0].relative_to(stage)),
                total=int(ran.group(1)) if ran else None,
                errors=errors, failures=failures)


def derive_status(protected_unchanged, controllers, segments):
    if not controllers:
        return "INCOMPLETE_NO_CONTROLLER_OUTPUT", ["no continuous-load receipt was found"]
    if not segments:
        return "INCOMPLETE_NO_MODEL_SEGMENT", ["no model segment material survived"]
    unreadable = [row["segment"] for row in segments if not row.get("rounds")]
    if unreadable and len(unreadable) == len(segments):
        return "INCOMPLETE_UNREADABLE_SEGMENTS", [f"no readable segment summary: {unreadable}"]
    last = controllers[-1]
    reasons = list(last.get("reasons") or [])
    if not protected_unchanged:
        return "REJECTED_PROTECTED_FILES_CHANGED", ["protected workspace files changed"]
    if last.get("status") == "PASS":
        return "ACCEPTED", []
    stopped = last.get("stopped_by") or "UNKNOWN"
    label = "NOT_ACCEPTED_" + re.sub(r"[^A-Z0-9]+", "_", str(stopped).upper())
    return label, reasons or [f"last window stopped by {stopped}"]


def main(stage, output):
    stage = Path(stage).resolve()
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=False)
    missing = [name for name in REQUIRED_MATERIALS if not (stage / name).is_file()]
    if missing:
        raise MissingMaterial(f"campaign materials missing: {missing}")

    caps = load(stage / "campaign.json")
    baseline = load(stage / "baseline-lock.json")
    identity = load(stage / "contract-identity.json")
    capacity = load(stage / "capacity-plan.json")
    preflight = load(stage / "PREFLIGHT.json") if (stage / "PREFLIGHT.json").is_file() else dict(status="NOT_RECORDED")
    work = stage / "workspace"
    current = {p.relative_to(work).as_posix(): digest(p) for p in work.rglob("*")
               if p.is_file() and "__pycache__" not in p.parts
               and "runtime-feedback" not in p.relative_to(work).parts}
    protected_unchanged = all(current.get(name) == value for name, value in baseline["files"].items())
    ledger = load(stage / "budget-ledger.json") if (stage / "budget-ledger.json").is_file() else None
    segments = segment_receipts(stage)
    controllers = controller_receipts(stage)
    candidate = work / "app/live_backup.py"
    candidate_present = candidate.is_file()
    status, reasons = derive_status(protected_unchanged, controllers, segments)
    accounting = None
    try:
        from campaign_accounting import campaign_accounting as accounting_report  # noqa: PLC0415
        accounting = accounting_report(stage)
    except Exception as error:  # accounting is evidence, not a gate
        accounting = dict(error=f"{type(error).__name__}: {error}")

    source_summary = None
    repositories = [path / "repository" for path in sorted(stage.glob("continuous-load*"))
                    if (path / "repository").is_dir()]
    repository_error = None
    if repositories:
        try:
            descriptor, members = repository_cut(repositories[-1])
            source_summary = summary(descriptor, members)
        except Exception as error:
            repository_error = f"{type(error).__name__}: {error}"

    evidence = dict(
        schema=2,
        status=status,
        reasons=reasons,
        campaign=str(stage),
        head=baseline.get("head"),
        contract_identity=identity,
        capacity_plan=capacity,
        preflight_status=preflight.get("status"),
        limits=dict(main_decisions=caps["main_decisions"], provider_attempts=caps["provider_attempts"],
                    tool_attempts=caps["tool_attempts"], estimated_cost_usd=caps["estimated_cost_usd"],
                    target_load_seconds=caps["target_load_seconds"],
                    install_budget=caps.get("install_budget")),
        accounting=accounting,
        provider_paid_calls=(len(ledger["attempts"]) if ledger else None),
        estimated_committed_usd=(ledger.get("committed_usd") if ledger else None),
        reserved_usd=(ledger.get("reserved_usd") if ledger else None),
        unknown_usd=(ledger.get("unknown_usd") if ledger else None),
        cap_stopped=(ledger.get("cap_stopped") if ledger else None),
        reserve_policy=(ledger.get("reserve_policy") if ledger else None),
        segments=segments,
        controller_windows=controllers,
        candidate=dict(present=candidate_present,
                       sha256=digest(candidate) if candidate_present else None),
        candidate_local_tests=candidate_tests(stage),
        protected_files=len(baseline["files"]),
        protected_unchanged=protected_unchanged,
        source_summary=source_summary,
        source_summary_error=repository_error,
        missing_materials=missing,
        remote_ci="NOT_RUN",
        submitted=False,
    )
    (output / "FINAL_STATUS.json").write_text(
        json.dumps(evidence, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    manifest = {p.relative_to(output).as_posix(): digest(p) for p in output.rglob("*") if p.is_file()}
    (output / "MANIFEST.json").write_text(
        json.dumps(dict(schema=2, files=manifest), ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(dict(status=status, reasons=reasons,
                          controller_windows=len(controllers), segments=len(segments),
                          candidate_sha256=evidence["candidate"]["sha256"],
                          protected_unchanged=protected_unchanged,
                          source_summary=source_summary, output=str(output)),
                     ensure_ascii=False))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        main(args.stage, args.output)
    except MissingMaterial as error:
        print(json.dumps(dict(status="INCOMPLETE", error=str(error)), ensure_ascii=False))
        raise SystemExit(2)

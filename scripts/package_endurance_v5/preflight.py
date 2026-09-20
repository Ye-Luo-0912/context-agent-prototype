"""Zero-provider V5 readiness checks.

The gate refuses to open a window unless the contract text, the judge and the
controller agree, the frozen workload provably fits the frozen archive bounds,
and the authorization budget is compatible with the action the task requires.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

import runner_grants  # noqa: E402
import workload  # noqa: E402
from oracle import calibration  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def contract_check(work, identity):
    """Contract text, oracle constants and recorded hashes must agree."""
    here = Path(__file__).resolve().parent
    bounds = workload.check_spec_bounds((work / "SPEC.md").read_text(encoding="utf-8"))
    if not bounds["agrees"]:
        raise ValueError(f"contract text and oracle constants disagree: {bounds['mismatches']}")
    recorded = identity.get("files", {})
    if sorted(recorded) != sorted(here_contract_files()):
        raise ValueError("contract identity does not cover the contract files")
    drifted = sorted(name for name, value in recorded.items()
                     if not (here / name).exists() or digest(here / name) != value)
    if drifted:
        raise ValueError(f"contract files changed after prepare: {drifted}")
    if digest(work / "SPEC.md") != identity["workspace"].get("SPEC.md"):
        raise ValueError("workspace SPEC.md changed after prepare")
    return bounds


def here_contract_files():
    from prepare import CONTRACT_FILES
    return list(CONTRACT_FILES)


def capacity_check(campaign, caps):
    plan = json.loads((campaign / "capacity-plan.json").read_bytes())
    recomputed = workload.closure_plan(install_budget=caps["install_budget"],
                                       writer_workers=caps["writer_workers"])
    if plan != recomputed:
        raise ValueError("recorded capacity plan does not match the frozen workload")
    workload.assert_satisfiable(plan)
    return plan


def preflight(stage):
    stage = Path(stage).resolve()
    caps = json.loads((stage / "campaign.json").read_bytes())
    if caps["kind"] != "v5_online_backup_continuous":
        raise ValueError("wrong campaign kind")
    if caps["deadline_epoch"] != caps["created_epoch"] + 6 * 3600:
        raise ValueError("deadline was modified")
    baseline = json.loads((stage / "baseline-lock.json").read_bytes())
    identity = json.loads((stage / "contract-identity.json").read_bytes())
    work = stage / "workspace"
    current = {p.relative_to(work).as_posix(): digest(p) for p in work.rglob("*")
               if p.is_file() and "__pycache__" not in p.parts and "runtime-feedback" not in p.relative_to(work).parts}
    if current != baseline["files"]:
        raise ValueError("protected workspace changed before start")
    bounds = contract_check(work, identity)
    plan = capacity_check(stage, caps)
    grants = runner_grants.campaign_grants(caps, python=sys.executable)
    authorization = runner_grants.compatibility(caps, grants)
    if not authorization["compatible"]:
        raise ValueError("authorization is not compatible with the frozen task: "
                         + "; ".join(authorization["problems"]))
    result = calibration(work)
    public = subprocess.run([sys.executable, "-B", "-m", "unittest", "discover", "-s", "app/tests", "-v"],
                            cwd=work, capture_output=True, text=True, timeout=120)
    if public.returncode:
        raise ValueError("protected baseline public tests failed: " + public.stdout[-2000:])
    return dict(status="READY", campaign_kind=caps["kind"], protected_files=len(baseline["files"]),
                contract_bounds=bounds, capacity_plan=plan, authorization=authorization,
                grants=grants, oracle_calibration=result, public_test_stdout=public.stdout[-4000:],
                provider_paid_calls=0)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        value = preflight(args.stage)
        args.out.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(value, ensure_ascii=False))
    except Exception as error:
        value = dict(status="NOT_READY", error_type=type(error).__name__, error=str(error), provider_paid_calls=0)
        args.out.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(value, ensure_ascii=False))
        raise SystemExit(1)

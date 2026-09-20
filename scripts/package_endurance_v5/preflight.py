"""Zero-provider V5 readiness checks."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from oracle import calibration

ROOT = Path(__file__).resolve().parents[2]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def preflight(stage):
    stage = Path(stage).resolve()
    caps = json.loads((stage / "campaign.json").read_bytes())
    if caps["kind"] != "v5_online_backup_continuous":
        raise ValueError("wrong campaign kind")
    if caps["deadline_epoch"] != caps["created_epoch"] + 6 * 3600:
        raise ValueError("deadline was modified")
    baseline = json.loads((stage / "baseline-lock.json").read_bytes())
    work = stage / "workspace"
    current = {p.relative_to(work).as_posix(): digest(p) for p in work.rglob("*")
               if p.is_file() and "__pycache__" not in p.parts and "runtime-feedback" not in p.parts}
    if current != baseline["files"]:
        raise ValueError("protected workspace changed before start")
    result = calibration(work)
    public = subprocess.run([sys.executable, "-B", "-m", "unittest", "discover", "-s", "app/tests", "-v"],
                            cwd=work, capture_output=True, text=True, timeout=120)
    if public.returncode:
        raise ValueError("protected baseline public tests failed: " + public.stdout[-2000:])
    return dict(status="READY", campaign_kind=caps["kind"], protected_files=len(baseline["files"]),
                oracle_calibration=result, public_test_stdout=public.stdout[-4000:],
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

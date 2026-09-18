"""Bounded, offline Runtime endurance admission probe; never calls a provider.

This implements the F12 preflight and its matched control, not the entire
20-fault/8-phase campaign. A failing admission probe prevents the paid journey.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

REPO = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    patch = subprocess.check_output(["git", "diff", "--binary"], cwd=REPO)
    (out / "runtime.patch").write_bytes(patch)
    # The shared checkout contains in-progress fixes, including untracked Rust
    # tests. HEAD + git diff alone cannot reconstruct this baseline.
    extras = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "--", "crates", "scripts"],
        cwd=REPO, text=True,
    ).splitlines()
    for name in extras:
        source = REPO / name
        if source.suffix in (".rs", ".py"):
            destination = out / "baseline-extra" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(source.read_bytes())
    sources = {}
    for crate in (REPO / "crates").iterdir():
        if crate.is_dir():
            for path in crate.rglob("*.rs"):
                sources[path.relative_to(REPO).as_posix()] = sha(path)
    baseline = {
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(),
        "dirty_diff_sha256": hashlib.sha256(patch).hexdigest(),
        "source_sha256": sources,
        "binaries": {p.name: sha(p) for p in (REPO / "target/debug").glob("agent-*.exe")},
        "python": sys.version,
        "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "scope": "offline F12 admission probe and free-lane control only",
    }
    (out / "baseline-lock.json").write_text(json.dumps(baseline, indent=2), encoding="utf-8")
    specs = [
        ("failure-resume-existing", "failure_resume", []),
        ("F12-free-control", "endurance_free_completion_lane_keeps_failed_turn_controls_responsive", ["--ignored"]),
        ("F12-occupied", "endurance_occupied_completion_gc_keeps_failed_turn_controls_responsive", []),
    ]
    results = []
    for label, test_filter, extra in specs:
        cmd = ["cargo", "test", "-p", "agent-runtime", "--test", "turn", test_filter, "--", *extra, "--nocapture"]
        env = os.environ.copy()
        env["ENDURANCE_F12_RECEIPT"] = str(out / (label + ".json"))
        started = time.monotonic()
        try:
            run = subprocess.run(cmd, cwd=REPO, env=env, capture_output=True, timeout=180)
            code, output = run.returncode, run.stdout + run.stderr
        except subprocess.TimeoutExpired as error:
            code = None
            output = (error.stdout or b"") + (error.stderr or b"") + b"\nHARNESS_TIMEOUT\n"
        (out / (label + ".log")).write_bytes(output)
        count = re.search(rb"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed", output)
        exercised = bool(count and sum(int(n) for n in count.groups()) >= (8 if label == "failure-resume-existing" else 1))
        result = {"case": label, "command": cmd, "exit": code, "exercised": exercised, "elapsed_seconds": round(time.monotonic() - started, 3)}
        results.append(result)
        print(json.dumps(result), flush=True)
        # A broken control cannot establish the occupied-lane counterexample.
        if (code != 0 or not exercised) and label != "F12-occupied":
            break
    passed = len(results) == len(specs) and all(row["exit"] == 0 and row["exercised"] for row in results)
    report = {
        "status": "F12_PREFLIGHT_PASS" if passed else "PREFLIGHT_FAILED",
        "paid_journey_started": False, "provider_attempts": 0,
        "full_campaign_implemented": False, "results": results,
        "remaining_faults": "NOT_EXERCISED_BY_THIS_RUNNER",
    }
    (out / "summary.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report), flush=True)
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

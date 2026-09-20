"""Create an exclusive V5 workspace; never reseed an existing campaign."""
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))

import oracle  # noqa: E402
import workload  # noqa: E402

CONTRACT_FILES = ("SPEC.md", "oracle.py", "workload.py", "continuous_load.py", "invoke.py",
                  "prepare.py", "preflight.py", "finalize.py", "run.py")


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def contract_identity(head, workspace_digests):
    """Bind the contract text, the judge and the controller into one identity."""
    here = Path(__file__).resolve().parent
    files = {name: digest(here / name) for name in CONTRACT_FILES}
    return dict(schema=1, head=head, files=files, workspace=workspace_digests,
                bounds=dict(members=oracle.ARCHIVE_MEMBER_BOUND,
                            member_bytes=oracle.MEMBER_BYTE_BOUND,
                            uncompressed_bytes=oracle.TOTAL_UNCOMPRESSED_BOUND,
                            archive_bytes=oracle.ARCHIVE_BYTE_BOUND))


def prepare(campaign):
    campaign = Path(campaign).resolve()
    campaign.mkdir(parents=True, exist_ok=False)
    now = time.time()
    caps = dict(schema=1, kind="v5_online_backup_continuous",
                created_epoch=now, deadline_epoch=now + 6 * 3600,
                main_decisions=260, provider_attempts=300,
                tool_attempts=900, input_tokens=8_000_000,
                output_tokens=220_000, estimated_cost_usd=2.0,
                api_protocol="chat", chat_thinking="disabled",
                model="deepseek-flash", target_load_seconds=7200,
                final_candidate_load_seconds=7200,
                install_budget=workload.INSTALL_BUDGET,
                writer_workers=workload.WRITER_WORKERS,
                backup_every=workload.BACKUP_EVERY)
    (campaign / "campaign.json").write_text(json.dumps(caps, indent=2) + "\n", encoding="utf-8")

    # The capacity plan is proven before the workspace exists: if the frozen
    # workload cannot be encoded inside the frozen archive bounds, no window may
    # open at all.
    plan = workload.closure_plan(install_budget=caps["install_budget"],
                                 writer_workers=caps["writer_workers"])
    workload.assert_satisfiable(plan)
    (campaign / "capacity-plan.json").write_text(
        json.dumps(plan, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    work = campaign / "workspace"
    work.mkdir()
    catalog = {}
    for index in range(workload.CATALOG_PACKAGES):
        name = f"pkg-{index:03d}"
        catalog[name] = []
        for version in range(1, workload.CATALOG_VERSIONS + 1):
            payload = workload.payload_bytes(name, version)
            blob = hashlib.sha256(payload).hexdigest()
            dependencies = {} if index == 0 else {
                f"pkg-{index - 1:03d}": {"min": version, "max": version + 1}
            }
            if index > 3 and index % 3 == 0:
                dependencies[f"pkg-{index - 3:03d}"] = {
                    "min": version, "max": version + 1
                }
            catalog[name].append(dict(version=version, sha256=blob, deps=dependencies))
            target = work / "fixtures/blobs" / blob
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(payload)
    (work / "fixtures/catalog.json").write_text(
        json.dumps(catalog, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
        encoding="utf-8")
    source = ROOT / "scripts/package_endurance_v3/fixture/app"
    for item in source.rglob("*"):
        if not item.is_file() or "__pycache__" in item.parts:
            continue
        target = work / "app" / item.relative_to(source)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(item, target)
    # The previously independently accepted offline snapshot implementation is
    # protected input and a useful serialization reference for the model.
    shutil.copyfile(ROOT / "scripts/package_endurance_v4/assisted/app/snapshot.py",
                    work / "app/snapshot.py")
    shutil.copyfile(Path(__file__).with_name("SPEC.md"), work / "SPEC.md")
    (work / "runtime-feedback").mkdir()
    (work / "runtime-feedback/latest.json").write_text(
        json.dumps(dict(status="PREPARED", batches=0, passed=0, failed=0)) + "\n",
        encoding="utf-8")

    files = {
        p.relative_to(work).as_posix(): digest(p)
        for p in work.rglob("*") if p.is_file() and "__pycache__" not in p.parts
        and "runtime-feedback" not in p.relative_to(work).parts
    }
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    binary = ROOT / "target/debug/agent-tui.exe"
    baseline = dict(schema=1, head=head, runtime_binary_sha256=digest(binary),
                    files=files, protected_candidate_paths=[
                        "app/live_backup.py", "app/tests/test_live_backup.py"
                    ], provenance="V5 fresh workspace from accepted v3 app and v4 snapshot reference")
    (campaign / "baseline-lock.json").write_text(
        json.dumps(baseline, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    (campaign / "v3-source-identity.json").write_text(
        json.dumps(dict(source=str(source), app_files=files), indent=2) + "\n", encoding="utf-8")
    (campaign / "contract-identity.json").write_text(
        json.dumps(contract_identity(head, files), indent=2, sort_keys=True) + "\n",
        encoding="utf-8")
    return dict(campaign=str(campaign), workspace=str(work), protected_files=len(files),
                capacity_satisfiable=plan["satisfiable"], **caps)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("campaign", type=Path)
    args = parser.parse_args()
    print(json.dumps(prepare(args.campaign), ensure_ascii=False))

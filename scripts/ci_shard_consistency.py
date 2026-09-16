#!/usr/bin/env python3
"""Linux cargo-test shard coverage gate (E3, review 980bbc77).

The ubuntu `test` matrix in .github/workflows/ci.yml selects crates with
explicit `-p` lists. A workspace member added to Cargo.toml but not added
to any shard would silently never run its Rust tests on Linux (a scoped
`cargo check`/`cargo build` elsewhere in the workflow does not run tests).
This script is the lightweight guard against that drift.

Checks (exit 0 only when all pass):

1. The `cargo_pkgs` lists of the ubuntu shards, parsed from the workflow's
   `test` matrix, contain no duplicate package across shards.
2. shard union + EXCLUDED == workspace members, where members come from
   `cargo metadata --no-deps` (the single source of truth for membership).
3. Every entry of EXCLUDED carries a non-empty reason.
4. The windows `full` entry keeps an empty `cargo_pkgs` (it runs the whole
   workspace and is not part of the Linux split).

EXCLUDED is for members that genuinely must not run in a Linux shard;
today it is empty. Silence here means "nothing is exempt", not "no check
ran" — the script fails loudly if parsing yields no ubuntu shards at all.

Run anywhere cargo works (CI runs it on ubuntu in the `check` job):
  python3 scripts/ci_shard_consistency.py
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CI = ROOT / ".github" / "workflows" / "ci.yml"

# Members that intentionally do not run in any Linux test shard, each with
# a stated reason. Must stay accurate: an entry here is exempt from the
# Linux cargo test coverage guarantee.
EXCLUDED: dict[str, str] = {}


def fail(problems: list[str]) -> None:
    for problem in problems:
        print(f"FAIL: {problem}")
    print("Linux test shards do not cover the workspace as expected.")
    sys.exit(1)


def workspace_members() -> set[str]:
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return {pkg["name"] for pkg in json.loads(out)["packages"]}


def parse_matrix_entries(text: str) -> list[dict[str, str]]:
    """Extract the `test` job's matrix `include` entries without a YAML dep."""
    job = re.split(r"(?m)^\s{2}test:\s*$", text)[1]
    job = re.split(r"(?m)^\s{2}\S.*:\s*$", job, maxsplit=1)[0]
    include = re.split(r"(?m)^\s+include:\s*$", job)[1]

    entries: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    for raw in include.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("- "):
            if current is not None:
                entries.append(current)
            current = {}
            line = line[2:]
        if current is None:
            continue
        key, sep, value = line.partition(":")
        if sep:
            current[key.strip()] = value.strip().strip('"')
    if current is not None:
        entries.append(current)
    return entries


def main() -> None:
    problems: list[str] = []
    entries = parse_matrix_entries(CI.read_text(encoding="utf-8"))

    ubuntu: dict[str, set[str]] = {}
    windows_full_seen = False
    for entry in entries:
        os_name = entry.get("os", "")
        part = entry.get("part", "")
        pkgs = {
            name
            for name in re.findall(r"-p\s+(\S+)", entry.get("cargo_pkgs", ""))
        }
        if os_name == "ubuntu-latest":
            if part in ubuntu:
                problems.append(f"duplicate ubuntu shard part {part}")
            if not pkgs:
                problems.append(f"ubuntu part {part} selects no packages")
            ubuntu[part] = pkgs
        elif os_name == "windows-latest" and part == "full":
            windows_full_seen = True
            if pkgs:
                problems.append(
                    "windows full entry must keep cargo_pkgs empty "
                    f"(found {sorted(pkgs)})"
                )

    if not ubuntu:
        fail(["no ubuntu shards were parsed from the test matrix"])
    if not windows_full_seen:
        problems.append("windows full entry missing; the split model changed")

    for part, pkgs in sorted(ubuntu.items()):
        print(f"ubuntu part {part}: {len(pkgs)} packages")

    overlaps = {
        (a, b): sorted(ubuntu[a] & ubuntu[b])
        for i, a in enumerate(ubuntu)
        for b in sorted(ubuntu)[i + 1 :]
    }
    for (a, b), shared in overlaps.items():
        if shared:
            problems.append(f"package(s) {shared} in both part {a} and part {b}")

    for name, reason in EXCLUDED.items():
        if not reason.strip():
            problems.append(f"EXCLUDED entry {name!r} has no stated reason")
        elif any(name in pkgs for pkgs in ubuntu.values()):
            problems.append(
                f"EXCLUDED entry {name!r} also runs in a shard; an exclusion "
                "must not contradict shard membership"
            )

    members = workspace_members()
    union = set().union(*ubuntu.values()) | set(EXCLUDED)
    missing = sorted(members - union)
    unknown = sorted(union - members)
    if missing:
        problems.append(
            "workspace member(s) missing from every Linux shard and not "
            f"excluded: {missing}"
        )
    if unknown:
        problems.append(f"shard/EXCLUDED name(s) not in the workspace: {unknown}")

    if problems:
        fail(problems)
    print(
        f"OK: {len(members)} workspace members = "
        + " ∪ ".join(f"part {part} ({len(pkgs)})" for part, pkgs in sorted(ubuntu.items()))
        + f" + {len(EXCLUDED)} explicit exclusion(s)"
    )


if __name__ == "__main__":
    main()

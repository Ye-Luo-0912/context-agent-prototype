#!/usr/bin/env python3
"""Document-consistency gate (run in CI and locally before doc commits).

Checks that the live documentation cannot drift from the machine-readable
state or from the repository itself:

1. docs/state.json parses and carries the required fields.
2. Every `_windows/<id>` window referenced by docs/STATUS.md exists on
   disk with a REPORT.md and manifest.json.
3. Stale-phrase blacklist on the live documents (README current status,
   docs/CURRENT.md, docs/STATUS.md): phrases that were true once and must
   never silently return.
4. Every relative markdown link in README.md and docs/*.md resolves.
5. The CI toolchain pin is present, and the workspace does not declare a
   conflicting rust-version.

Exit 0 only when every check passes.
"""

import json
import os
import re
import sys
import urllib.parse

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REQUIRED_STATE_FIELDS = ["schema", "head_commit", "m15", "product_stage"]
STALE_PHRASES = {
    "README.md": [
        "seven v4 valid FAIL",
        "M15 remains open",
        "latest is 10/12",
    ],
    "docs/CURRENT.md": [
        "M15 remains open",
        "working-tree candidate",
    ],
    "docs/STATUS.md": [
        "M15 formally remains open",
    ],
}
LIVE_DOCS = [
    "README.md",
    "AGENTS.md",
    "docs/CURRENT.md",
    "docs/STATUS.md",
    "docs/ROADMAP.md",
    "docs/NEXT_TASKS.md",
    "docs/CONFIGURATION.md",
    "docs/RECOVERY_RUNBOOK.md",
    "docs/COMPATIBILITY.md",
    "docs/CONTEXT_FRAME_V1.md",
    "docs/EXECUTION_MODEL.md",
    "docs/reviews/2026-09-05-code-review.md",
]


def check_state_json(violations):
    path = os.path.join(ROOT, "docs", "state.json")
    try:
        with open(path, encoding="utf-8") as handle:
            state = json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        violations.append(f"state.json unreadable: {error}")
        return None
    for field in REQUIRED_STATE_FIELDS:
        if field not in state:
            violations.append(f"state.json missing required field: {field}")
    return state


def check_windows(status_text, violations):
    window_root = os.path.join(
        ROOT, "crates", "agent-eval", "evidence", "m15-window", "_windows"
    )
    for match in re.finditer(r"_windows/(\d+)", status_text):
        window_id = match.group(1)
        directory = os.path.join(window_root, window_id)
        if not os.path.isdir(directory):
            violations.append(f"STATUS references missing window: _windows/{window_id}")
            continue
        for required in ("REPORT.md", "manifest.json"):
            if not os.path.isfile(os.path.join(directory, required)):
                violations.append(
                    f"window _windows/{window_id} is missing {required}"
                )


def check_stale_phrases(violations):
    for relative, phrases in STALE_PHRASES.items():
        path = os.path.join(ROOT, relative)
        if not os.path.isfile(path):
            violations.append(f"live document missing: {relative}")
            continue
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        for phrase in phrases:
            if phrase in text:
                violations.append(f"{relative} contains stale phrase: {phrase!r}")


def check_links(violations):
    link_pattern = re.compile(r"\]\(([^)\s#]+)(?:#[^)\s]*)?\)")
    for relative in LIVE_DOCS:
        path = os.path.join(ROOT, relative)
        if not os.path.isfile(path):
            violations.append(f"live document missing: {relative}")
            continue
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        base = os.path.dirname(path)
        for match in link_pattern.finditer(text):
            target = match.group(1)
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            resolved = os.path.normpath(
                os.path.join(base, urllib.parse.unquote(target))
            )
            if not os.path.exists(resolved):
                violations.append(f"{relative}: broken link -> {target}")


def check_toolchain(violations):
    ci_path = os.path.join(ROOT, ".github", "workflows", "ci.yml")
    with open(ci_path, encoding="utf-8") as handle:
        ci = handle.read()
    if "toolchain: 1.97.1" not in ci:
        violations.append("ci.yml no longer pins toolchain 1.97.1; update this gate")
    for manifest in ("Cargo.toml",):
        path = os.path.join(ROOT, manifest)
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        match = re.search(r"^rust-version\s*=\s*\"([^\"]+)\"", text, re.M)
        if match and match.group(1) != "1.97.1":
            violations.append(
                f"{manifest} declares rust-version {match.group(1)}; "
                "CI pins 1.97.1 — align them"
            )


def main():
    violations = []
    state = check_state_json(violations)
    with open(os.path.join(ROOT, "docs", "STATUS.md"), encoding="utf-8") as handle:
        status_text = handle.read()
    if state is not None:
        closing = state.get("m15", {}).get("closing_window", {})
        report = closing.get("report")
        if report and not os.path.isfile(os.path.join(ROOT, report)):
            violations.append(f"state.json closing window report missing: {report}")
    check_windows(status_text, violations)
    check_stale_phrases(violations)
    check_links(violations)
    check_toolchain(violations)

    if violations:
        print("document-consistency gate FAILED:")
        for violation in violations:
            print(f"  - {violation}")
        sys.exit(1)
    print("document-consistency gate: OK "
          f"({len(LIVE_DOCS)} live docs, links and state agree)")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Document-structure gate (run in CI and locally before doc commits).

This gate verifies MECHANICAL structure only. It does NOT verify that the
documentation is semantically true, that described code exists, that a CI
run covers a claimed implementation, or that the current route is the one
the maintainer intends. Those facts are owned by the entry documents
themselves (docs/CURRENT.md, docs/NEXT_TASKS.md) and the code-review
process, not by this script.

Checks:

1. docs/state.json parses and carries the v2 navigation fields (the v1
   current-status fields live in the archived snapshot).
2. Entry roles: each entry file exists and is non-empty; docs/CURRENT.md
   and docs/NEXT_TASKS.md cross-reference each other; AGENTS.md points to
   the entry pair. The archived entry snapshot exists alongside its
   baseline provenance note.
3. Historical `_windows/<id>` references in docs/STATUS.md resolve on disk
   (kept because STATUS historically carried window links).
4. Every relative markdown link in the live documents resolves to a file.
5. The CI toolchain pin is present, and the workspace does not declare a
   conflicting rust-version.

Exit 0 only when every check passes. Success means structure/links/toolchain
checks passed — nothing more.
"""

import json
import os
import re
import sys
import urllib.parse

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REQUIRED_STATE_FIELDS = ["schema", "role", "maintained_by", "historical_snapshots"]
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
    "docs/reviews/2026-09-06-deep-audit/REVIEW.md",
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


def check_entry_roles(violations):
    """Cheap structural checks on the entry pair, per the 2026-09-14 doc
    migration: the entry files exist, cross-reference each other, and the
    stable-conventions file points at them. This is structure, not truth."""
    entries = {
        "AGENTS.md": ["docs/CURRENT.md", "docs/NEXT_TASKS.md"],
        "docs/CURRENT.md": ["NEXT_TASKS.md"],
        "docs/NEXT_TASKS.md": ["CURRENT.md"],
        "docs/ROADMAP.md": ["CURRENT.md", "NEXT_TASKS.md"],
        "docs/STATUS.md": ["CURRENT.md", "NEXT_TASKS.md"],
    }
    for relative, refs in entries.items():
        path = os.path.join(ROOT, relative)
        if not os.path.isfile(path):
            violations.append(f"entry document missing: {relative}")
            continue
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        if len(text.strip()) == 0:
            violations.append(f"entry document is empty: {relative}")
        for ref in refs:
            if ref not in text:
                violations.append(
                    f"{relative} no longer references {ref} (entry cross-link)"
                )
    archive = os.path.join(
        ROOT, "docs", "archive", "entries-2026-09-13-2b43186b"
    )
    for name in (
        "AGENTS.md",
        "CURRENT.md",
        "NEXT_TASKS.md",
        "ROADMAP.md",
        "STATUS.md",
        "state.json",
        "state-PROVENANCE.md",
    ):
        if not os.path.isfile(os.path.join(archive, name)):
            violations.append(f"archived entry snapshot missing: {name}")


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
        # The v1 m15 closing-window evidence lives in the archived snapshot;
        # keep checking the referenced report exists so the archive stays
        # honest, but read it from the snapshot rather than live state.
        snapshots = state.get("historical_snapshots", {})
        for meta in snapshots.values():
            archive_dir = meta.get("path")
            if not archive_dir:
                violations.append("state.json snapshot entry missing path")
                continue
            archive_state = os.path.join(ROOT, archive_dir, "state.json")
            if not os.path.isfile(archive_state):
                violations.append(f"archived state snapshot missing: {archive_state}")
                continue
            with open(archive_state, encoding="utf-8") as handle:
                archived = json.load(handle)
            closing = archived.get("m15", {}).get("closing_window", {})
            report = closing.get("report")
            if report and not os.path.isfile(os.path.join(ROOT, report)):
                violations.append(
                    f"archived state closing window report missing: {report}"
                )
    check_windows(status_text, violations)
    check_stale_phrases(violations)
    check_entry_roles(violations)
    check_links(violations)
    check_toolchain(violations)

    if violations:
        print("document-consistency gate FAILED:")
        for violation in violations:
            print(f"  - {violation}")
        sys.exit(1)
    print("document-consistency gate: OK "
          f"({len(LIVE_DOCS)} live docs; structure/links/toolchain checks passed)")


if __name__ == "__main__":
    main()

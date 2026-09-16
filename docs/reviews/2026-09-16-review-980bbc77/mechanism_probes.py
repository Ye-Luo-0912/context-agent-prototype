#!/usr/bin/env python3
"""Small mechanism checks for review 980bbc77; NOT Rust/.NET repository tests.

No network, provider request, tool execution, or unsafe FFI call is performed.
The truncation check ports the reviewed pure head/tail transformation. The CI
check uses the package lists transcribed from the fixed-SHA manifest/workflow.
"""
from __future__ import annotations
import hashlib
import json
import struct
from pathlib import Path

SHA = "980bbc77f4086ebf8848f5c9afa16ce22fafe39f"

def clip(value: str, limit: int, field: str, location: str | None) -> str:
    if len(value) <= limit:
        return value
    reference = f" Full output: {location[:256]}." if location else ""
    marker = (
        f"...[output broker truncated {field} from {len(value)} chars "
        f"to the {limit}-char cap.{reference}]..."
    )
    remaining = max(0, limit - len(marker))
    head = remaining // 2
    tail = remaining - head
    return value[:head] + marker + (value[-tail:] if tail else "")

def main() -> None:
    sentinel = "REQUIRED_MIDDLE_SENTINEL"
    lines = [f"{i:04d}: " + "x" * 80 for i in range(1, 401)]
    lines[199] = "0200: " + sentinel + "x" * 55
    body = "sample.rs @ fixed-revision\n" + "\n".join(lines)
    metadata = {"start_line": 1, "end_line": 400, "covers_file": True}
    clipped = clip(body, 16_000, "model_content", "artifact://mechanism-only/full.txt")
    complete = not (metadata.get("truncated", False) or metadata.get("window_truncated", False))
    assert sentinel in body and sentinel not in clipped
    assert len(clipped) == 16_000 and complete and metadata["covers_file"]

    a, b = 2**53, 2**53 + 1
    assert a != b and a < 2**63 and b < 2**63
    assert struct.pack(">d", float(a)) == struct.pack(">d", float(b))
    # Only the integer-valued binary64 collision is demonstrated here; this
    # does NOT implement or test the repository's complete JCS serializer.
    canon_a = json.dumps({"n": int(float(a))}, separators=(",", ":"))
    canon_b = json.dumps({"n": int(float(b))}, separators=(",", ":"))
    assert canon_a == canon_b

    workspace = set("agent-contracts agent-platform-protocol context-simple context-baselines context-contextcore agent-process agent-capability-process agent-context-service agent-workspace tool-runtime agent-conformance agent-storage agent-core agent-runtime agent-replay agent-eval provider-openai agent-compose agent-tui agent-host".split())
    part1 = set("context-simple agent-tui agent-workspace tool-runtime agent-process agent-conformance agent-replay".split())
    part2 = set("agent-runtime agent-eval agent-contracts agent-platform-protocol context-baselines context-contextcore agent-capability-process agent-context-service agent-storage agent-core provider-openai agent-compose".split())
    missing = workspace - part1 - part2
    assert missing == {"agent-host"}

    result = {
        "baseline": SHA,
        "evidence_kind": "Python mechanism checks; not compiled repository regressions",
        "body_projection": {
            "original_chars": len(body), "projected_chars": len(clipped),
            "middle_sentinel_retained": sentinel in clipped,
            "unchanged_metadata_implies_complete": complete,
            "unchanged_metadata_implies_whole_file": metadata["covers_file"],
        },
        "integer_domain": {
            "left": a, "right": b, "distinct_exact_integers": a != b,
            "same_binary64": True, "narrow_example_canonical": canon_a,
            "same_example_sha256": hashlib.sha256(canon_a.encode()).hexdigest(),
            "not_a_cryptographic_hash_collision": True,
        },
        "ci_selection": {
            "workspace_packages": len(workspace),
            "linux_selected_packages": len(part1 | part2),
            "missing": sorted(missing), "overlap": sorted(part1 & part2),
        },
        "small_output_budget_observation": {
            "declared_chars": 1,
            "returned_chars": len(clip("abc", 1, "model_content", None)),
        },
        "not_executed": ["Rust builds/tests", ".NET builds/tests", "PTY tests", "provider calls", "unsafe mkfifo fixture"],
    }
    target = Path(__file__).with_name("MECHANISM_CHECKS.json")
    target.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False, indent=2))

if __name__ == "__main__":
    main()

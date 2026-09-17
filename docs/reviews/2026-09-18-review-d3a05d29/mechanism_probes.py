#!/usr/bin/env python3
"""Standalone mechanism checks; NOT the repository's Rust/.NET test suite.

Read-only analysis of a fixed source snapshot. Probes use temporary local
files only, do not execute repository code, and do not call providers.
"""
from __future__ import annotations
import codecs
import json
import platform
import tempfile
from pathlib import Path

BASELINE = "d3a05d297295da1ece66b00245fa027f2ee12852"
SCAN_CAP = 8 * 1024 * 1024
CHUNK_CAP = 4000


def normalized_tokens(text: str) -> list[str]:
    def trim(token: str) -> str:
        left, right = 0, len(token)
        while left < right and not (token[left].isalnum() or token[left] in "-_"):
            left += 1
        while right > left and not (token[right - 1].isalnum() or token[right - 1] in "-_"):
            right -= 1
        return token[left:right].lower()
    return [trim(token) for token in text.split()]


def retention_protection(text: str) -> bool:
    tokens = normalized_tokens(text)
    protected = {
        "keep", "keeps", "keeping", "retain", "retains", "retaining",
        "preserve", "preserves", "preserving", "remains", "remain",
        "still", "never", "dont", "not",
    }
    return any(token in protected for token in tokens) or any(
        pair == ["do", "not"] for pair in (tokens[i:i+2] for i in range(len(tokens)-1))
    )


def entities(text: str) -> list[str]:
    punctuation = ",;()[]{}<>\"'`?!"
    selected = []
    for raw in text.split():
        token = raw.strip(punctuation)
        if len(token.encode("utf-8")) >= 3 and (
            "." in token or "/" in token or "::" in token or "_" in token
            or any("A" <= c <= "Z" for c in token)
        ):
            selected.append(token)
            if len(selected) == 24:
                break
    return sorted(set(selected))


def direct_object_withdrawal(text: str, older: str) -> bool:
    old = {entity.lower() for entity in entities(older)}
    tokens = normalized_tokens(text)
    for i, token in enumerate(tokens):
        if token not in {"drop", "remove", "switch", "discard", "abandon"}:
            continue
        for candidate in tokens[i + 1:i + 4]:
            if candidate in {"to", "the", "a", "an", "our", "this"}:
                continue
            if candidate in old:
                return True
            break
    return False


def negation_probe() -> dict:
    old = "Use AuthService.rs with a 5-second timeout"
    cases = []
    for text, should_withdraw in [
        ("Remove AuthService.rs", True),
        ("Do not remove AuthService.rs", False),
        ("Don't remove AuthService.rs", False),
        ("Don’t remove AuthService.rs", False),
        ("Never remove AuthService.rs", False),
    ]:
        protected = retention_protection(text)
        direct = direct_object_withdrawal(text, old)
        same_entity = bool(set(entities(old)) & set(entities(text)))
        # In these cases the decision and replacement-cue gates both match
        # "remove ". The direct-object branch alone suffices for the OR proof.
        would_queue = "remove " in text.lower() and same_entity and not protected and direct
        cases.append({"incoming": text, "tokens": normalized_tokens(text),
                      "retention_protected": protected, "direct_object_matches": direct,
                      "same_entity": same_entity, "would_queue_via_direct_object_branch": would_queue,
                      "intended_withdrawal": should_withdraw})
    assert cases[0]["would_queue_via_direct_object_branch"] is True
    assert cases[1]["would_queue_via_direct_object_branch"] is False
    assert cases[2]["would_queue_via_direct_object_branch"] is True
    assert cases[3]["would_queue_via_direct_object_branch"] is True
    assert cases[4]["would_queue_via_direct_object_branch"] is False
    return {"evidence_kind": "PYTHON_PORT_OF_SELECTED_PREDICATES", "older": old, "cases": cases,
            "limitation": "No ContextEngine ingest/maintain or Rust regression was executed."}


def eof_probe() -> dict:
    cases = []
    with tempfile.TemporaryDirectory(prefix="agent-review-eof-") as temp:
        path = Path(temp) / "artifact.log"
        for size in (SCAN_CAP - 1, SCAN_CAP, SCAN_CAP + 1):
            path.write_bytes(b"x" * (size - 1) + b"\n")
            with path.open("rb") as stream:
                scanned = stream.read(SCAN_CAP)
                lookahead = stream.read(1)
            remaining = SCAN_CAP - len(scanned)
            inferred_complete = remaining > 0
            cases.append({"file_bytes": size, "scanned_bytes": len(scanned),
                          "remaining_take_budget": remaining,
                          "current_completion_formula": inferred_complete,
                          "actual_eof_at_scan_boundary": not lookahead})
    assert cases[0]["current_completion_formula"] is True
    assert cases[1]["current_completion_formula"] is False
    assert cases[1]["actual_eof_at_scan_boundary"] is True
    assert cases[2]["actual_eof_at_scan_boundary"] is False
    # Exact-cap single-line artifact, after the last content span was
    # delivered: first_unshown=2 but scanned.complete stays false.
    positions = []
    start, end = 1, 200
    for _ in range(4):
        first_unshown = 2 if start == 1 else start
        in_window_unshown = first_unshown <= min(end, 1)
        has_more = True  # !scanned.complete
        next_line = first_unshown if in_window_unshown else end + 1
        positions.append({"request_start": start, "request_end": end,
                          "has_more": has_more, "next_start_line": next_line,
                          "source_lines": 1})
        start, end = next_line, next_line + 199
    assert [p["next_start_line"] for p in positions] == [201, 401, 601, 801]
    return {"evidence_kind": "REAL_TEMP_FILE_IO_PLUS_COVERAGE_FORMULA_PORT",
            "cases": cases, "post_content_continuations": positions,
            "limitation": "Does not run Tokio Take, ArtifactReadTool, Broker or Runtime."}


def utf8_probe() -> dict:
    cases = []
    for text in ("¢", "界", "🧪"):
        raw = b"a" * (CHUNK_CAP - 1) + text.encode("utf-8")
        chunks = [raw[i:i+CHUNK_CAP] for i in range(0, len(raw), CHUNK_CAP)]
        rendered_separately = "".join(chunk.decode("utf-8", errors="replace") for chunk in chunks)
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        rendered_stream = "".join(decoder.decode(chunk, final=False) for chunk in chunks)
        rendered_stream += decoder.decode(b"", final=True)
        assert b"".join(chunks) == raw
        assert rendered_stream == raw.decode("utf-8")
        assert text not in rendered_separately
        assert "\ufffd" in rendered_separately
        cases.append({"character": text, "raw_hex": text.encode().hex(),
                      "chunk_lengths": [len(c) for c in chunks],
                      "per_chunk_replacement_count": rendered_separately.count("\ufffd"),
                      "valid_character_preserved_per_chunk": text in rendered_separately,
                      "raw_artifact_bytes_preserved": b"".join(chunks) == raw,
                      "incremental_decoder_preserves_text": rendered_stream == raw.decode()})
    return {"evidence_kind": "REAL_PYTHON_UTF8_DECODE_ON_PRODUCTION_SIZED_BYTE_BOUNDARIES",
            "cases": cases, "limitation": "Rust pump_stream/StreamCapture were inspected, not executed."}


def main() -> None:
    out = {
        "baseline": BASELINE,
        "execution_environment": {"python": platform.python_version(), "system": platform.system()},
        "rust_repository_tests": "NOT_RUN", "dotnet_tests": "NOT_RUN",
        "vendor_calls": "NOT_RUN", "repository_modified": False,
        "H1_negation": negation_probe(),
        "H2_cold_semantics": {"evidence_kind": "STATIC_CALL_CHAIN_ONLY", "rust_test": "NOT_RUN"},
        "H3_exact_cap_eof": eof_probe(),
        "H4_wal_prewrite_refusal": {"evidence_kind": "STATIC_CALL_CHAIN_ONLY", "rust_test": "NOT_RUN"},
        "H5_stream_utf8": utf8_probe(),
    }
    path = Path(__file__).with_name("MECHANISM_RESULTS.json")
    path.write_text(json.dumps(out, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"results": str(path), "checks": "passed as mechanism checks", "rust_tests": "NOT_RUN"}, ensure_ascii=False))

if __name__ == "__main__":
    main()

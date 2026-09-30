"""One-request MiMo Chat compatibility probe; no prompt or response logging."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import urllib.error
import urllib.request

from terminal_bench_pilot.credential_relay import CredentialRelay


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report-dir", type=Path, required=True)
    parser.add_argument("--protocol", required=True)
    args = parser.parse_args(argv)
    credential = sys.stdin.readline().rstrip("\r\n")
    if not credential or len(credential) > 512:
        raise RuntimeError("missing or invalid in-memory trial credential")
    args.report_dir.mkdir(exist_ok=False, parents=True)
    (args.report_dir / "identity.json").write_text(json.dumps({
        "protocol": args.protocol,
        "model": "mimo-v2.6-flash",
        "api_base_url": "https://api.xiaomimimo.com/v1",
        "price_source": "https://mimo.mi.com/docs/en-US/pricing",
        "price_checked_date": "2026-09-26",
        "max_provider_requests": 1,
        "max_input_tokens": 10_000,
        "max_output_tokens": 256,
        "estimated_usd_cap": 1,
        "tool_name_mode": "reversible_api_safe_alias",
        "probe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    }, indent=2), encoding="utf-8")
    relay = CredentialRelay(
        upstream="https://api.xiaomimimo.com/v1", credential=credential,
        model="mimo-v2.6-flash", max_requests=1, max_output_tokens=256,
        max_input_tokens_total=10_000, max_output_tokens_total=256,
        max_peak_miss_usd=1, input_price_milli_usd_per_million=140,
        output_price_milli_usd_per_million=280, api_safe_tool_names=True,
        prefer_max_completion_tokens=True, omit_stream_options=True,
        min_request_interval_ms=750,
    )
    outcome: dict = {"provider": "mimo-v2.6-flash", "status": "NOT_RUN",
                     "upstream_call_count": 0, "tool_name_round_trip": "NOT_OBSERVED"}
    try:
        request = {
            "model": "mimo-v2.6-flash", "stream": True,
            "stream_options": {"include_usage": True}, "max_tokens": 256,
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content":
                          "Call fs.read once with path api/example.py, then stop."}],
            "tools": [{"type": "function", "function": {
                "name": "fs.read", "description": "Read one source file",
                "parameters": {"type": "object", "properties": {
                    "path": {"type": "string"}}, "required": ["path"]},
            }}],
        }
        wire = json.dumps(request, separators=(",", ":")).encode()
        response = urllib.request.urlopen(urllib.request.Request(
            relay.base_url + "/chat/completions", data=wire,
            headers={"Authorization": "Bearer " + relay.token},
        ), timeout=120).read()
        tool_names = []
        for line in response.splitlines():
            if not line.startswith(b"data: "):
                continue
            try:
                event = json.loads(line[6:])
            except ValueError:
                continue
            for choice in event.get("choices", []):
                for call in (choice.get("delta", {}).get("tool_calls") or []):
                    name = (call.get("function") or {}).get("name")
                    if name:
                        tool_names.append(name)
        if tool_names:
            outcome["tool_name_round_trip"] = (
                "PASS" if all(name == "fs.read" for name in tool_names) else "FAIL"
            )
        outcome["status"] = "PASS" if b"data: [DONE]" in response else "STREAM_INCOMPLETE"
    except urllib.error.HTTPError as error:
        outcome["status"] = "HTTP_ERROR"
        outcome["relay_status_code"] = error.code
    except Exception as error:
        outcome["status"] = type(error).__name__
    finally:
        relay.close()
        relay.save_receipt(args.report_dir / "relay.json")
        outcome["upstream_call_count"] = len(relay.attempts)
        if relay.attempts:
            outcome["usage_state"] = relay.attempts[0]["state"]
            outcome["upstream_status_code"] = relay.attempts[0].get("upstream_status_code")
            outcome["input_tokens"] = relay.input_tokens_committed
            outcome["output_tokens"] = relay.output_tokens_committed
        (args.report_dir / "result.json").write_text(
            json.dumps(outcome, indent=2), encoding="utf-8"
        )
        print(json.dumps(outcome), flush=True)
    return 0 if outcome["status"] == "PASS" and outcome.get("usage_state") == "completed" \
        and outcome["tool_name_round_trip"] != "FAIL" else 2


if __name__ == "__main__":
    raise SystemExit(main())

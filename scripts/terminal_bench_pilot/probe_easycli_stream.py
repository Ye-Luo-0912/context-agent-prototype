"""Make one bounded streaming tool-call probe through the local EasyCLI relay.

Only protocol flags and usage counts are printed; model text and tool arguments
are never printed or saved. No account credential is read by this process.
"""
from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.credential_relay import CredentialRelay


def main() -> int:
    relay = CredentialRelay(
        upstream="http://127.0.0.1:8317/v1",
        credential="",
        model="gpt-6-luna",
        max_requests=1,
        max_output_tokens=128,
        max_input_tokens_total=16_000,
        max_output_tokens_total=128,
        input_price_milli_usd_per_million=100,
        cached_input_price_milli_usd_per_million=10,
        output_price_milli_usd_per_million=500,
        api_safe_tool_names=True,
        upstream_timeout_secs=120,
        api_protocol="responses",
        allow_unauthenticated_loopback=True,
    )
    body = {
        "model": "gpt-6-luna",
        "stream": True,
        "max_output_tokens": 128,
        "reasoning": {"effort": "max"},
        "input": [{"role": "user", "content": [{
            "type": "input_text", "text": "Call probe_result with value exactly ok.",
        }]}],
        "tools": [{
            "type": "function", "name": "probe_result",
            "description": "Return one short probe value.",
            "parameters": {"type": "object", "properties": {
                "value": {"type": "string", "enum": ["ok"]},
            }, "required": ["value"], "additionalProperties": False},
            "strict": True,
        }],
        "tool_choice": {"type": "function", "name": "probe_result"},
        "parallel_tool_calls": False,
    }
    result = {"status": "NOT_RUN", "model": "gpt-6-luna",
              "api_protocol": "responses", "reasoning_effort": "max",
              "max_output_tokens": 128}
    try:
        request = urllib.request.Request(
            relay.base_url + "/responses",
            data=json.dumps(body, separators=(",", ":")).encode(),
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json",
                     "Accept": "text/event-stream"},
        )
        with urllib.request.urlopen(request, timeout=130) as response:
            result["http_status"] = response.status
            payload = response.read(4 * 1024 * 1024 + 1)
        completed = False
        function_seen = False
        name_restored = False
        call_id_present = False
        if len(payload) <= 4 * 1024 * 1024:
            for line in payload.splitlines():
                if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
                    continue
                try:
                    event = json.loads(line[6:])
                except (ValueError, UnicodeDecodeError):
                    continue
                if not isinstance(event, dict):
                    continue
                completed |= event.get("type") == "response.completed"
                item = event.get("item")
                if isinstance(item, dict) and item.get("type") == "function_call":
                    function_seen = True
                    name_restored |= item.get("name") == "probe_result"
                    call_id_present |= isinstance(item.get("call_id"), str) and bool(
                        item["call_id"]
                    )
        attempt = relay.attempts[0] if relay.attempts else {}
        result.update(
            status="PASS" if (completed and function_seen and name_restored
                              and call_id_present and attempt.get("state") == "completed")
            else "PROTOCOL_UNVERIFIED",
            response_completed=completed,
            function_call_seen=function_seen,
            function_name_restored=name_restored,
            call_id_present=call_id_present,
            relay_attempt_state=attempt.get("state"),
            input_tokens=attempt.get("input_tokens"),
            cached_input_tokens=attempt.get("cached_input_tokens"),
            output_tokens=attempt.get("output_tokens"),
            relay_budget_unknown=relay.budget_unknown,
        )
    except urllib.error.HTTPError as error:
        attempt = relay.attempts[0] if relay.attempts else {}
        result.update(status="HTTP_ERROR", http_status=error.code,
                      upstream_status=attempt.get("upstream_status_code"),
                      relay_budget_unknown=relay.budget_unknown)
    except Exception as error:
        result.update(status="PROBE_ERROR", error_type=type(error).__name__,
                      relay_budget_unknown=relay.budget_unknown)
    finally:
        relay.close()
    print(json.dumps(result, separators=(",", ":")), flush=True)
    return 0 if result["status"] == "PASS" else 2


if __name__ == "__main__":
    raise SystemExit(main())

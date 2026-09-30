"""One bounded Pinaic GPT-6 Luna Responses tool-call probe.

The API key is read from stdin and held only in memory. Output contains
protocol metadata and token counts, never provider bodies or tool arguments.
"""
from __future__ import annotations

import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.credential_relay import CredentialRelay


BASE_URL = "https://api.pinaic.com/v1"
MODEL = "gpt-6-luna"
TOOL_NAME = "probe_result"
OUTPUT_CAP = 1024
PROBE_TEXT = "Call the required function with the value exactly 'ok'."


def main() -> int:
    credential = sys.stdin.readline().rstrip("\r\n")
    if not credential or len(credential) > 512:
        raise RuntimeError("missing or invalid in-memory trial credential")

    relay = CredentialRelay(
        upstream=BASE_URL,
        credential=credential,
        model=MODEL,
        max_requests=1,
        max_output_tokens=OUTPUT_CAP,
        max_input_tokens_total=16_000,
        max_output_tokens_total=OUTPUT_CAP,
        max_peak_miss_usd=0.02,
        input_price_milli_usd_per_million=400,
        cached_input_price_milli_usd_per_million=40,
        output_price_milli_usd_per_million=2_000,
        api_safe_tool_names=True,
        min_request_interval_ms=1_000,
        upstream_timeout_secs=300,
        api_protocol="responses",
    )
    body = {
        "model": MODEL,
        "stream": True,
        "max_output_tokens": OUTPUT_CAP,
        "reasoning": {"effort": "max"},
        "input": [{"role": "user", "content": [
            {"type": "input_text", "text": PROBE_TEXT},
        ]}],
        "tools": [{
            "type": "function",
            "name": TOOL_NAME,
            "description": "Return the required short probe value.",
            "parameters": {
                "type": "object",
                "properties": {"value": {"type": "string", "enum": ["ok"]}},
                "required": ["value"],
                "additionalProperties": False,
            },
            "strict": True,
        }],
        "tool_choice": {"type": "function", "name": TOOL_NAME},
        "parallel_tool_calls": False,
    }

    result: dict = {
        "provider_base_url": BASE_URL,
        "model": MODEL,
        "api_protocol": "responses",
        "reasoning_effort": "max",
        "request_cap": OUTPUT_CAP,
        "status": "NOT_RUN",
    }
    http_status: int | None = None
    try:
        request = urllib.request.Request(
            relay.base_url + "/responses",
            data=json.dumps(body, separators=(",", ":")).encode(),
            headers={
                "Authorization": "Bearer " + relay.token,
                "Content-Type": "application/json",
                "Accept": "text/event-stream",
            },
        )
        with urllib.request.urlopen(request, timeout=310) as response:
            http_status = response.status
            payload = response.read(4 * 1024 * 1024 + 1)
        if len(payload) > 4 * 1024 * 1024:
            result["status"] = "RESPONSE_TOO_LARGE"
        else:
            event_types: set[str] = set()
            function_call_seen = False
            function_name_restored = False
            call_id_present = False
            for line in payload.splitlines():
                if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
                    continue
                try:
                    event = json.loads(line[6:])
                except (ValueError, UnicodeDecodeError):
                    continue
                if not isinstance(event, dict):
                    continue
                event_type = event.get("type")
                if isinstance(event_type, str):
                    event_types.add(event_type)
                item = event.get("item")
                if isinstance(item, dict) and item.get("type") == "function_call":
                    function_call_seen = True
                    function_name_restored |= item.get("name") == TOOL_NAME
                    call_id_present |= isinstance(item.get("call_id"), str) and bool(item["call_id"])
                if event_type == "response.function_call_arguments.done":
                    function_name_restored |= event.get("name") == TOOL_NAME
            attempt = relay.attempts[0] if relay.attempts else {}
            usage = attempt.get("usage") if isinstance(attempt, dict) else None
            usage = usage if isinstance(usage, dict) else {}
            input_tokens = usage.get("input_tokens", usage.get("prompt_tokens"))
            output_tokens = usage.get("output_tokens", usage.get("completion_tokens"))
            cached_input_tokens = usage.get("cached_input_tokens", 0)
            result.update(
                status="PASS" if (
                    "response.completed" in event_types
                    and function_call_seen
                    and function_name_restored
                    and call_id_present
                    and isinstance(input_tokens, int)
                    and isinstance(output_tokens, int)
                ) else "PROTOCOL_UNVERIFIED",
                http_status=http_status,
                event_count=len(event_types),
                function_call_seen=function_call_seen,
                function_name_restored=function_name_restored,
                call_id_present=call_id_present,
                response_completed="response.completed" in event_types,
                usage_observed=(
                    isinstance(input_tokens, int) and isinstance(output_tokens, int)
                ),
                input_tokens=input_tokens if isinstance(input_tokens, int) else None,
                cached_input_tokens=(
                    cached_input_tokens if isinstance(cached_input_tokens, int) else None
                ),
                output_tokens=output_tokens if isinstance(output_tokens, int) else None,
                relay_attempt_state=attempt.get("state"),
                relay_budget_unknown=relay.budget_unknown,
            )
    except urllib.error.HTTPError as error:
        result.update(status="HTTP_ERROR", http_status=error.code)
        attempt = relay.attempts[0] if relay.attempts else {}
        upstream_status = attempt.get("upstream_status_code")
        if isinstance(upstream_status, int):
            result["upstream_status"] = upstream_status
        result["relay_budget_unknown"] = relay.budget_unknown
    except Exception as error:
        result.update(status="PROBE_ERROR", error_type=type(error).__name__)
        result["relay_budget_unknown"] = relay.budget_unknown
    finally:
        relay.close()

    print(json.dumps(result, separators=(",", ":")), flush=True)
    return 0 if result["status"] == "PASS" else 2


if __name__ == "__main__":
    raise SystemExit(main())

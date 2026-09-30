"""One bounded Anthropic Messages-format tool-call probe for Pinaic.

Reads the key from stdin and reports protocol/usage metadata only. Provider
error messages, response text, and tool arguments are never printed or saved.
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.probe_proxy_models import safe_error_metadata


BASE_URL = "https://api.pinaic.com/v1"
MODEL = "gpt-6-luna"
TOOL_NAME = "probe_result"
MAX_TOKENS = 128


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--auth-style", choices=("x-api-key", "bearer"), default="x-api-key")
    args = parser.parse_args()
    credential = sys.stdin.readline().rstrip("\r\n")
    if not credential or len(credential) > 512:
        raise RuntimeError("missing or invalid in-memory trial credential")

    body = {
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "system": "You are a protocol compatibility probe. Keep responses minimal.",
        "messages": [{
            "role": "user",
            "content": 'Call the required tool with value exactly "ok".',
        }],
        "tools": [{
            "name": TOOL_NAME,
            "description": "Return the short protocol probe value.",
            "input_schema": {
                "type": "object",
                "properties": {"value": {"type": "string", "enum": ["ok"]}},
                "required": ["value"],
                "additionalProperties": False,
            },
        }],
        "tool_choice": {"type": "tool", "name": TOOL_NAME},
    }
    result: dict = {
        "provider_base_url": BASE_URL,
        "model": MODEL,
        "api_protocol": "anthropic-messages",
        "auth_style": args.auth_style,
        "request_max_tokens": MAX_TOKENS,
        "status": "NOT_RUN",
    }
    headers = {
        "anthropic-version": "2023-06-01",
        "Content-Type": "application/json",
        "Accept": "application/json",
    }
    if args.auth_style == "bearer":
        headers["Authorization"] = "Bearer " + credential
    else:
        headers["x-api-key"] = credential
    request = urllib.request.Request(
        BASE_URL + "/messages",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers=headers,
    )
    opener = urllib.request.build_opener(NoRedirect())
    try:
        with opener.open(request, timeout=90) as response:
            payload = response.read(65_537)
            result["http_status"] = response.status
        if len(payload) > 65_536:
            result["status"] = "RESPONSE_TOO_LARGE"
        else:
            try:
                value = json.loads(payload)
            except (ValueError, UnicodeDecodeError):
                result["status"] = "UNPARSEABLE_RESPONSE"
            else:
                blocks = value.get("content", []) if isinstance(value, dict) else []
                tool_uses = [
                    block for block in blocks
                    if isinstance(block, dict) and block.get("type") == "tool_use"
                ]
                usage = value.get("usage", {}) if isinstance(value, dict) else {}
                result.update(
                    status="HTTP_OK",
                    response_type=value.get("type") if isinstance(value, dict) else None,
                    response_model_matches=(
                        isinstance(value, dict) and value.get("model") == MODEL
                    ),
                    stop_reason=(
                        value.get("stop_reason") if isinstance(value, dict) else None
                    ),
                    tool_use_count=len(tool_uses),
                    expected_tool_seen=any(
                        block.get("name") == TOOL_NAME for block in tool_uses
                    ),
                    tool_use_id_present=any(
                        isinstance(block.get("id"), str) and bool(block["id"])
                        for block in tool_uses
                    ),
                    input_tokens=(
                        usage.get("input_tokens") if isinstance(usage, dict) else None
                    ),
                    output_tokens=(
                        usage.get("output_tokens") if isinstance(usage, dict) else None
                    ),
                )
    except urllib.error.HTTPError as error:
        result["status"] = "HTTP_ERROR"
        result["http_status"] = error.code
        result["error_content_type"] = error.headers.get("Content-Type")
        try:
            result.update(safe_error_metadata(error.read(65_537)))
        except Exception:
            pass
        error.close()
    except Exception as error:
        result.update(status="PROBE_ERROR", error_type=type(error).__name__)

    print(json.dumps(result, separators=(",", ":")), flush=True)
    return 0 if result.get("status") == "HTTP_OK" else 2


if __name__ == "__main__":
    raise SystemExit(main())

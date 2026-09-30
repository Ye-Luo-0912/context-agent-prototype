"""Read-only authenticated model-list probe for an OpenAI-compatible relay."""
from __future__ import annotations

import argparse
import json
import re
import sys
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


PRICE_FIELDS = {
    "pricing", "price", "input_price", "output_price", "prompt_price",
    "completion_price", "input_cost", "output_cost", "currency",
}
SAFE_ERROR_LABEL = re.compile(r"[A-Za-z][A-Za-z0-9_.-]{0,63}\Z")


def safe_error_metadata(body: bytes) -> dict[str, str]:
    """Return only machine labels from an error body; never expose its message."""
    try:
        payload = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return {}
    if not isinstance(payload, dict):
        return {}
    error = payload.get("error", payload)
    if not isinstance(error, dict):
        return {}
    result = {}
    for field, output_name in (("code", "error_code"), ("type", "error_type")):
        value = error.get(field)
        if (isinstance(value, str) and not value.startswith("sk-")
                and SAFE_ERROR_LABEL.fullmatch(value)):
            result[output_name] = value
    return result


def safe_prices(model: dict) -> dict:
    result = {}
    for key in PRICE_FIELDS:
        value = model.get(key)
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            result[key] = value
        elif isinstance(value, str) and key == "currency" and len(value) <= 12:
            result[key] = value
        elif isinstance(value, dict):
            numeric = {
                str(subkey): subvalue
                for subkey, subvalue in value.items()
                if isinstance(subvalue, (int, float)) and not isinstance(subvalue, bool)
            }
            if numeric:
                result[key] = numeric
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--model", default="gpt-6-luna")
    parser.add_argument("--max-response-bytes", type=int, default=1_048_576)
    args = parser.parse_args(argv)
    credential = sys.stdin.readline().rstrip("\r\n")
    if not credential or len(credential) > 512:
        raise RuntimeError("missing or invalid in-memory credential")

    url = args.base_url.rstrip("/") + "/models"
    request = urllib.request.Request(url, headers={
        "Authorization": "Bearer " + credential,
        "Accept": "application/json",
    })
    output: dict = {
        "base_url": args.base_url.rstrip("/"),
        "requested_model": args.model,
        "status": "NOT_RUN",
    }
    try:
        opener = urllib.request.build_opener(NoRedirect())
        with opener.open(request, timeout=20) as response:
            body = response.read(args.max_response_bytes + 1)
            if len(body) > args.max_response_bytes:
                output["status"] = "RESPONSE_TOO_LARGE"
            else:
                payload = json.loads(body)
                rows = payload.get("data", []) if isinstance(payload, dict) else []
                matched = [
                    row for row in rows
                    if isinstance(row, dict) and row.get("id") == args.model
                ]
                output.update(
                    status="OK",
                    model_count=len(rows),
                    exact_model_available=bool(matched),
                    model_prices=safe_prices(matched[0]) if matched else {},
                    model_fields=sorted(matched[0].keys()) if matched else [],
                )
    except urllib.error.HTTPError as error:
        output["status"] = "HTTP_ERROR"
        output["http_status"] = error.code
        output["error_content_type"] = error.headers.get("Content-Type")
        try:
            error_body = error.read(min(args.max_response_bytes, 65_536) + 1)
        except Exception:
            error_body = b""
        output.update(safe_error_metadata(error_body))
        error.close()
    except Exception as error:
        output["status"] = type(error).__name__
    print(json.dumps(output, separators=(",", ":")), flush=True)
    return 0 if output.get("status") == "OK" else 2


if __name__ == "__main__":
    raise SystemExit(main())

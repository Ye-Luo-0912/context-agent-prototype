"""One-shot, bounded EasyCLI Responses probe for explicit standard safeguards.

Default mode performs only a loopback /v1/models check.  Pass --execute for
exactly one Responses request.  The request and model output are never printed
or saved; only protocol state and usage counts are written to the report.
"""
from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.credential_relay import CredentialRelay


ROOT = Path(__file__).resolve().parents[2]
REPORT = ROOT / "docs/experiments/terminal-bench-pilot/TB50_DAYBREAK_STANDARD_ONESHOT_20260929.json"
UPSTREAM = "http://127.0.0.1:8317/v1"
MODEL = "gpt-6-luna"


def check_local_model() -> tuple[bool, int | None, bool]:
    request = urllib.request.Request(UPSTREAM + "/models", method="GET")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=5) as response:
            if response.status != 200:
                return False, response.status, False
            payload = json.loads(response.read(1024 * 1024))
        models = payload.get("data", []) if isinstance(payload, dict) else []
        present = any(isinstance(item, dict) and item.get("id") == MODEL
                      for item in models)
        # The model was directly accepted for 18 TB48 calls although this
        # proxy's /models surface omits its exact ID.  Keep the omission visible
        # without treating it as a denial or overriding direct request evidence.
        return True, response.status, present
    except urllib.error.HTTPError as error:
        return False, error.code, False
    except Exception:
        return False, None, False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute", action="store_true",
                        help="send exactly one bounded request after loopback preflight")
    args = parser.parse_args()

    loopback_ready, models_status, model_list_has_model = check_local_model()
    report: dict[str, object] = {
        "identity": "TB50_EASYCLI_DAYBREAK_STANDARD_ONESHOT_20260929",
        "model": MODEL,
        "api_protocol": "responses",
        "requested_access_program": "standard",
        "provider_attempt_cap": 1,
        "input_token_cap": 8192,
        "output_token_cap": 16,
        "max_output_tokens_per_request": 16,
        "retry_cap": 0,
        "upstream_timeout_seconds": 60,
        "loopback_models_http_status": models_status,
        "model_list_has_exact_model_id": model_list_has_model,
        "prior_direct_successes_same_model": 18,
        "provider_contacted": False,
        "status": "STATIC_READY_NO_PROVIDER_CALLS" if loopback_ready else "BLOCKED_LOCAL_PREFLIGHT",
    }
    if not loopback_ready or not args.execute:
        print(json.dumps(report, separators=(",", ":")))
        return 0 if loopback_ready else 2

    relay = CredentialRelay(
        upstream=UPSTREAM,
        credential="",
        model=MODEL,
        max_requests=1,
        max_body_bytes=4096,
        max_output_tokens=16,
        max_input_tokens_total=8192,
        max_output_tokens_total=16,
        input_price_milli_usd_per_million=400,
        cached_input_price_milli_usd_per_million=40,
        output_price_milli_usd_per_million=2000,
        upstream_timeout_secs=60,
        api_protocol="responses",
        allow_unauthenticated_loopback=True,
        max_retryable_408_retries=0,
    )
    body = {
        "model": MODEL,
        "stream": True,
        "max_output_tokens": 16,
        "reasoning": {"effort": "max"},
        "access_programs": {"cyber": "standard"},
        "input": "Reply with OK.",
    }
    try:
        request = urllib.request.Request(
            relay.base_url + "/responses",
            data=json.dumps(body, separators=(",", ":")).encode(),
            headers={"Authorization": "Bearer " + relay.token,
                     "Content-Type": "application/json",
                     "Accept": "text/event-stream"},
        )
        with urllib.request.urlopen(request, timeout=70) as response:
            report["http_status"] = response.status
            payload = response.read(1024 * 1024 + 1)
        completed = False
        failed = False
        if len(payload) <= 1024 * 1024:
            for line in payload.splitlines():
                if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
                    continue
                try:
                    event = json.loads(line[6:])
                except (ValueError, UnicodeDecodeError):
                    continue
                if isinstance(event, dict):
                    completed |= event.get("type") == "response.completed"
                    failed |= event.get("type") == "response.failed"
        report["response_completed"] = completed
        report["response_failed_event"] = failed
        report["response_bytes_within_local_read_cap"] = len(payload) <= 1024 * 1024
    except urllib.error.HTTPError as error:
        report["http_status"] = error.code
        report["client_http_error"] = True
    except Exception as error:
        report["client_error_type"] = type(error).__name__
    finally:
        relay.close()

    attempt = relay.attempts[0] if relay.attempts else {}
    report.update({
        "provider_contacted": bool(relay.attempts),
        "relay_attempt_state": attempt.get("state"),
        "upstream_status_code": attempt.get("upstream_status_code"),
        "input_tokens": attempt.get("input_tokens"),
        "cached_input_tokens": attempt.get("cached_input_tokens"),
        "output_tokens": attempt.get("output_tokens"),
        "input_tokens_reserved": relay.input_tokens_reserved,
        "output_tokens_reserved": relay.output_tokens_reserved,
        "input_tokens_committed": relay.input_tokens_committed,
        "output_tokens_committed": relay.output_tokens_committed,
        "budget_unknown": relay.budget_unknown,
        "retry_used": relay.retryable_408_retries_used,
        "observed_cyber_program": attempt.get("observed_cyber_program"),
        "response_access_program_metadata_captured": (
            attempt.get("observed_cyber_program") is not None
        ),
        "status": (
            "REQUEST_COMPLETED_USAGE_SETTLED"
            if report.get("response_completed") and attempt.get("state") == "completed"
            else "STOP_UNKNOWN" if relay.budget_unknown
            else "PROTOCOL_UNVERIFIED"
        ),
    })
    REPORT.parent.mkdir(parents=True, exist_ok=True)
    REPORT.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    relay.save_receipt(REPORT.with_name("TB50_DAYBREAK_STANDARD_RELAY_RECEIPT_20260929.json"))
    print(json.dumps(report, separators=(",", ":")))
    return 0 if report["status"] == "REQUEST_COMPLETED_USAGE_SETTLED" else 2


if __name__ == "__main__":
    raise SystemExit(main())

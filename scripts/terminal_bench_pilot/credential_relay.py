"""Host-only, trial-scoped credential relay. No request/response body logging.

The task receives a revocable nonce, never the upstream account credential.
This is an HTTP capability with finite request, input-byte and output-token
bounds, not a read-only sandbox for programs inside the task container.
"""
from __future__ import annotations

import base64
import copy
import hashlib
import hmac
import http.client
import json
import secrets
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _tool_alias(name: str) -> str:
    """Injective API-safe alias; never change the Runtime/Core tool identity."""
    if not isinstance(name, str) or not name:
        raise ValueError("tool name must be a non-empty string")
    alias = "t_" + base64.urlsafe_b64encode(name.encode("utf-8")).decode("ascii").rstrip("=")
    if len(alias) > 64:
        raise ValueError("tool name cannot fit the provider's 64-character limit")
    return alias


def _rewrite_request_tools(
    request: dict, *, prefer_max_completion_tokens: bool = False,
    omit_stream_options: bool = False, api_protocol: str = "chat",
    single_file_edit_patch: bool = False,
) -> tuple[bytes, dict[str, str]]:
    """Rewrite only typed function-name slots, including prior tool calls."""
    aliases: dict[str, str] = {}

    def rename(function):
        if not isinstance(function, dict) or "name" not in function:
            return
        original = function["name"]
        alias = _tool_alias(original)
        aliases[alias] = original
        function["name"] = alias

    def rename_direct(item):
        if not isinstance(item, dict) or not isinstance(item.get("name"), str):
            return
        original = item["name"]
        alias = _tool_alias(original)
        aliases[alias] = original
        item["name"] = alias

    if api_protocol == "responses":
        for tool in request.get("tools", []):
            if isinstance(tool, dict) and tool.get("type") == "function":
                if single_file_edit_patch and tool.get("name") == "edit.patch":
                    parameters = tool.get("parameters")
                    properties = (
                        parameters.get("properties")
                        if isinstance(parameters, dict) else None
                    )
                    files = properties.get("files") if isinstance(properties, dict) else None
                    if not isinstance(files, dict) or files.get("type") != "array":
                        raise ValueError("single-file edit.patch guard requires a files array schema")
                    existing_max = files.get("maxItems")
                    if type(existing_max) is int and existing_max < 1:
                        raise ValueError("edit.patch schema has an invalid maxItems bound")
                    files["maxItems"] = 1
                    tool["strict"] = True
                    current_description = tool.get("description")
                    if isinstance(current_description, str):
                        tool["description"] = (
                            current_description.rstrip()
                            + " This trial enforces exactly one file per edit.patch call. "
                              "Submit a separate call for each file."
                        )
                    tool["strict"] = True
                rename_direct(tool)
        input_items = request.get("input")
        if isinstance(input_items, list):
            for item in input_items:
                if isinstance(item, dict) and item.get("type") == "function_call":
                    rename_direct(item)
        choice = request.get("tool_choice")
        if isinstance(choice, dict) and choice.get("type") == "function":
            rename_direct(choice)
        elif isinstance(choice, dict) and choice.get("type") == "allowed_tools":
            for item in choice.get("tools", []):
                if isinstance(item, dict) and item.get("type") == "function":
                    rename_direct(item)
    else:
        for tool in request.get("tools", []):
            if isinstance(tool, dict) and tool.get("type") == "function":
                rename(tool.get("function"))
        for message in request.get("messages", []):
            if isinstance(message, dict):
                for call in (message.get("tool_calls") or []):
                    if isinstance(call, dict) and call.get("type") == "function":
                        rename(call.get("function"))
        choice = request.get("tool_choice")
        if isinstance(choice, dict) and choice.get("type") == "function":
            rename(choice.get("function"))
        if prefer_max_completion_tokens and "max_tokens" in request:
            request["max_completion_tokens"] = request.pop("max_tokens")
        if omit_stream_options:
            request.pop("stream_options", None)
    return json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode(), aliases


def _rewrite_sse_line(
    line: bytes, aliases: dict[str, str], *, api_protocol: str = "chat",
) -> bytes:
    if not line.startswith(b"data: ") or line[6:].rstrip(b"\r") == b"[DONE]":
        return line
    event = json.loads(line[6:])
    if not isinstance(event, dict):
        return line
    if api_protocol == "responses":
        event_type = event.get("type")
        if event_type in {"response.output_item.added", "response.output_item.done"}:
            item = event.get("item")
            if (isinstance(item, dict) and item.get("type") == "function_call"
                    and isinstance(item.get("name"), str)):
                item["name"] = aliases.get(item["name"], item["name"])
        elif event_type == "response.function_call_arguments.done":
            if isinstance(event.get("name"), str):
                event["name"] = aliases.get(event["name"], event["name"])
        return b"data: " + json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode()
    if event.get("choices") is None:
        event["choices"] = []
    for choice in (event.get("choices") or []):
        if not isinstance(choice, dict):
            continue
        if choice.get("delta") is None:
            choice["delta"] = {}
        for branch_name in ("delta", "message"):
            branch = choice.get(branch_name)
            if not isinstance(branch, dict):
                continue
            if branch.get("tool_calls") is None:
                branch["tool_calls"] = []
            for call in (branch.get("tool_calls") or []):
                function = call.get("function") if isinstance(call, dict) else None
                if isinstance(function, dict) and isinstance(function.get("name"), str):
                    function["name"] = aliases.get(function["name"], function["name"])
    return b"data: " + json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode()


def _usage_from_event(event: dict, api_protocol: str) -> dict | None:
    usage = event.get("usage")
    if api_protocol == "responses":
        response = event.get("response")
        if isinstance(response, dict) and isinstance(response.get("usage"), dict):
            usage = response["usage"]
    if isinstance(usage, dict):
        input_details = usage.get("input_tokens_details")
        if not isinstance(input_details, dict):
            input_details = usage.get("prompt_tokens_details")
        cached_input_tokens = usage.get(
            "cached_input_tokens", usage.get("cache_read_input_tokens")
        )
        if cached_input_tokens is None and isinstance(input_details, dict):
            cached_input_tokens = input_details.get("cached_tokens", 0)
        usage = {
            **usage,
            "prompt_tokens": usage.get("input_tokens", usage.get("prompt_tokens")),
            "completion_tokens": usage.get("output_tokens", usage.get("completion_tokens")),
            "cached_input_tokens": 0 if cached_input_tokens is None else cached_input_tokens,
        }
    return usage if isinstance(usage, dict) else None


def _record_response_terminal_event(record: dict, event: dict) -> str | None:
    event_type = event.get("type")
    if event_type in {"response.completed", "response.failed", "response.incomplete"}:
        record["response_terminal_event"] = event_type
        record["response_completed"] = event_type == "response.completed"
        return event_type
    return None


def _local_stream_error_code(error: Exception) -> str | None:
    """Classify only our own bounded failures; never persist exception text."""
    if isinstance(error, json.JSONDecodeError):
        return "malformed_sse_json"
    if type(error) is ValueError:
        return {
            "response bound exceeded": "response_bytes_limit",
            "SSE line bound exceeded": "sse_line_bytes_limit",
            "rewritten response bound exceeded": "rewritten_response_bytes_limit",
        }.get(str(error), "other_value_error")
    return None


def _repair_missing_tool_ids(lines: list[bytes]) -> tuple[list[bytes], int]:
    """Give MiMo's id-less call an identity before its first streamed delta.

    Inspect the whole bounded response first: a later provider id always wins.
    This changes transport correlation only; Runtime/Core still validate the
    original tool name, arguments, and effect authorization.
    """
    events: list[dict | None] = []
    first: dict[tuple[int, int], tuple[int, int, int]] = {}
    provider_ids: set[str] = set()
    has_id: set[tuple[int, int]] = set()
    malformed_id: set[tuple[int, int]] = set()
    for line_index, line in enumerate(lines):
        event = None
        if line.startswith(b"data: ") and line[6:].rstrip(b"\r") != b"[DONE]":
            value = json.loads(line[6:])
            if isinstance(value, dict):
                event = value
                for choice_position, choice in enumerate(value.get("choices") or []):
                    if not isinstance(choice, dict):
                        continue
                    choice_index = choice.get("index", 0)
                    if type(choice_index) is not int or choice_index < 0:
                        continue
                    delta = choice.get("delta")
                    if not isinstance(delta, dict):
                        continue
                    for call_position, call in enumerate(delta.get("tool_calls") or []):
                        if not isinstance(call, dict):
                            continue
                        tool_index = call.get("index")
                        if type(tool_index) is not int or tool_index < 0:
                            continue
                        key = (choice_index, tool_index)
                        first.setdefault(key, (line_index, choice_position, call_position))
                        call_id = call.get("id")
                        if isinstance(call_id, str) and call_id:
                            provider_ids.add(call_id)
                            has_id.add(key)
                        elif call_id is not None:
                            malformed_id.add(key)
        events.append(event)

    repaired = 0
    for key, (line_index, choice_position, call_position) in first.items():
        if key in has_id or key in malformed_id:
            continue
        call = events[line_index]["choices"][choice_position]["delta"]["tool_calls"][call_position]
        while True:
            synthetic = f"call_relay_{secrets.token_hex(12)}_{key[1]}"
            if synthetic not in provider_ids:
                break
        provider_ids.add(synthetic)
        call["id"] = synthetic
        repaired += 1

    output = []
    for line, event in zip(lines, events):
        output.append(
            b"data: " + json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode()
            if event is not None else line
        )
    return output, repaired


class CredentialRelay:
    def __init__(self, *, upstream: str, credential: str, model: str,
                 bind: str = "127.0.0.1", max_requests: int = 40,
                 max_body_bytes: int = 262144, max_output_tokens: int = 8192,
                 max_input_tokens_total: int | None = None,
                 max_output_tokens_total: int | None = None,
                 max_peak_miss_usd: int | None = None,
                 input_price_milli_usd_per_million: int = 300,
                 cached_input_price_milli_usd_per_million: int | None = None,
                 output_price_milli_usd_per_million: int = 1200,
                 api_safe_tool_names: bool = False,
                 prefer_max_completion_tokens: bool = False,
                 omit_stream_options: bool = False,
                 min_request_interval_ms: int = 0,
                 upstream_timeout_secs: int = 120,
                 repair_missing_tool_call_ids: bool = False,
                 api_protocol: str = "chat",
                 allow_unauthenticated_loopback: bool = False,
                 max_retryable_408_retries: int = 0,
                 standard_cyber_safeguards: bool = False,
                 single_file_edit_patch: bool = False):
        if (not credential and not allow_unauthenticated_loopback) or min(
            max_requests, max_body_bytes, max_output_tokens
        ) <= 0:
            raise ValueError("credential and positive trial limits are required")
        if allow_unauthenticated_loopback:
            target = urllib.parse.urlsplit(upstream)
            if (credential or target.scheme != "http"
                    or target.hostname != "127.0.0.1" or not target.port
                    or target.username or target.password):
                raise ValueError("anonymous upstream must be HTTP on 127.0.0.1")
        if not 0 <= max_retryable_408_retries <= 4:
            raise ValueError("retryable HTTP 408 retries must be within 0..4")
        if max_peak_miss_usd is not None and max_peak_miss_usd <= 0:
            raise ValueError("peak-miss USD cap must be positive")
        cached_input_price = (
            input_price_milli_usd_per_million
            if cached_input_price_milli_usd_per_million is None
            else cached_input_price_milli_usd_per_million
        )
        if min(input_price_milli_usd_per_million, cached_input_price,
               output_price_milli_usd_per_million) <= 0:
            raise ValueError("positive provider prices are required")
        if not 0 <= min_request_interval_ms <= 60_000:
            raise ValueError("request interval must be within 0..60000 ms")
        if not 30 <= upstream_timeout_secs <= 600:
            raise ValueError("upstream timeout must be within 30..600 seconds")
        if repair_missing_tool_call_ids and not api_safe_tool_names:
            raise ValueError("tool-id repair requires the bounded API-safe SSE path")
        if api_protocol not in {"chat", "responses"}:
            raise ValueError("API protocol must be chat or responses")
        if standard_cyber_safeguards and api_protocol != "responses":
            raise ValueError("standard cyber safeguards require the Responses API")
        if single_file_edit_patch and (api_protocol != "responses" or not api_safe_tool_names):
            raise ValueError(
                "single-file edit.patch guard requires Responses API-safe tool names"
            )
        self.api_protocol = api_protocol
        self.standard_cyber_safeguards = standard_cyber_safeguards
        self.single_file_edit_patch = single_file_edit_patch
        self.upstream = upstream.rstrip("/") + (
            "/responses" if api_protocol == "responses" else "/chat/completions"
        )
        self.credential, self.model = credential, model
        self.token = "trial-" + secrets.token_urlsafe(32)
        self.max_requests, self.max_body_bytes = max_requests, max_body_bytes
        self.max_output_tokens = max_output_tokens
        self.max_input_tokens_total = max_input_tokens_total
        self.max_output_tokens_total = max_output_tokens_total
        self.max_peak_miss_usd = max_peak_miss_usd
        # A milli-USD per million tokens is one nano-USD per token.
        self.input_price_milli_usd_per_million = input_price_milli_usd_per_million
        self.cached_input_price_milli_usd_per_million = cached_input_price
        self.cached_input_price_is_assumed = cached_input_price_milli_usd_per_million is None
        self.output_price_milli_usd_per_million = output_price_milli_usd_per_million
        self.api_safe_tool_names = api_safe_tool_names
        self.prefer_max_completion_tokens = prefer_max_completion_tokens
        self.omit_stream_options = omit_stream_options
        self.min_request_interval_ms = min_request_interval_ms
        self.upstream_timeout_secs = upstream_timeout_secs
        self.repair_missing_tool_call_ids = repair_missing_tool_call_ids
        self.next_upstream_at = 0.0
        self.input_tokens_reserved = 0
        self.output_tokens_reserved = 0
        self.input_tokens_committed = 0
        self.cached_input_tokens_committed = 0
        self.output_tokens_committed = 0
        self.budget_unknown = False
        self.budget_exceeded = False
        self.max_retryable_408_retries = max_retryable_408_retries
        self.retryable_408_retries_used = 0
        self.retryable_408_retry_hash: bytes | None = None
        self.retryable_408_recovered = False
        self.non_retryable_unknown = False
        self.rejections: dict[str, int] = {}
        self.attempts: list[dict] = []
        self.lock = threading.Lock()
        self.idle = threading.Condition(self.lock)
        self.active_calls = 0
        self.closed = False
        relay = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def reject(self, code, reason=None):
                self.send_response(code)
                self.send_header("Content-Length", "0")
                if reason is not None:
                    self.send_header("X-Relay-Stop-Reason", reason)
                self.end_headers()

            def do_POST(self):
                self.connection.settimeout(120)
                expected_path = ("/v1/responses" if relay.api_protocol == "responses"
                                 else "/v1/chat/completions")
                if self.path != expected_path:
                    self.reject(404)
                    return
                if not hmac.compare_digest(self.headers.get("Authorization", ""),
                                           "Bearer " + relay.token):
                    self.reject(401)
                    return
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if not 0 < length <= relay.max_body_bytes:
                        raise ValueError()
                    body = self.rfile.read(length)
                    if len(body) != length:
                        raise ValueError()
                    request = json.loads(body)
                    if not isinstance(request, dict) or request.get("model") != relay.model:
                        raise ValueError()
                    if request.get("stream") is not True:
                        raise ValueError()
                    if relay.standard_cyber_safeguards:
                        access_programs = request.get("access_programs")
                        if access_programs is None:
                            access_programs = {}
                        if not isinstance(access_programs, dict):
                            raise ValueError()
                        access_programs = dict(access_programs)
                        access_programs["cyber"] = "standard"
                        request["access_programs"] = access_programs
                        body = json.dumps(
                            request, ensure_ascii=False, separators=(",", ":"),
                        ).encode()
                    if relay.api_protocol == "chat":
                        if request.get("n", 1) != 1 or "best_of" in request:
                            raise ValueError()
                        cap = request.get("max_tokens", request.get("max_completion_tokens"))
                        if "max_tokens" in request and "max_completion_tokens" in request:
                            raise ValueError()
                    else:
                        cap = request.get("max_output_tokens")
                    if type(cap) is not int or not 0 < cap <= relay.max_output_tokens:
                        raise ValueError()
                    aliases = {}
                    if relay.api_safe_tool_names:
                        body, aliases = _rewrite_request_tools(
                            request,
                            prefer_max_completion_tokens=relay.prefer_max_completion_tokens,
                            omit_stream_options=relay.omit_stream_options,
                            api_protocol=relay.api_protocol,
                            single_file_edit_patch=relay.single_file_edit_patch,
                        )
                    if len(body) > relay.max_body_bytes:
                        raise ValueError()
                except (ValueError, TypeError):
                    self.reject(400)
                    return
                body_digest = hashlib.sha256(body).digest()
                # A byte-sized reservation plus framing margin avoids the old
                # bytes/4 underestimate. DeepSeek does not document a strict
                # wire-bytes-to-billed-tokens upper bound, so this remains a
                # conservative admission estimate, not a billing guarantee.
                input_reservation = len(body) + 4096
                with relay.lock:
                    input_limit_hit = (
                        relay.max_input_tokens_total is not None
                        and relay.input_tokens_committed + relay.input_tokens_reserved + input_reservation
                        > relay.max_input_tokens_total
                    )
                    output_limit_hit = (
                        relay.max_output_tokens_total is not None
                        and relay.output_tokens_committed + relay.output_tokens_reserved + cap
                        > relay.max_output_tokens_total
                    )
                    cost_limit_hit = (
                        relay.max_peak_miss_usd is not None
                        and (relay.input_price_milli_usd_per_million
                             * (relay.input_tokens_committed + relay.input_tokens_reserved
                                + input_reservation)
                             + relay.output_price_milli_usd_per_million
                             * (relay.output_tokens_committed + relay.output_tokens_reserved
                                + cap)) > relay.max_peak_miss_usd * 1_000_000_000
                    )
                    pending_retry = relay.retryable_408_retry_hash is not None
                    retrying_same_408 = (
                        pending_retry
                        and hmac.compare_digest(
                            relay.retryable_408_retry_hash or b"", body_digest,
                        )
                    )
                    unknown_blocked = (
                        relay.non_retryable_unknown
                        or (pending_retry and not retrying_same_408)
                        or (relay.budget_unknown
                            and not relay.retryable_408_recovered
                            and not retrying_same_408)
                    )
                    stop_reason = (
                        "closed" if relay.closed else
                        "usage_unknown" if unknown_blocked else
                        "budget_exceeded" if relay.budget_exceeded else
                        "request_limit" if len(relay.attempts) >= relay.max_requests else
                        "input_limit" if input_limit_hit else
                        "output_limit" if output_limit_hit else
                        "price_limit" if cost_limit_hit else None
                    )
                    if stop_reason is not None:
                        relay.rejections[stop_reason] = relay.rejections.get(stop_reason, 0) + 1
                    else:
                        if retrying_same_408:
                            # Only the exact original request may consume the
                            # single retry granted for a pre-response HTTP 408.
                            relay.retryable_408_retry_hash = None
                        send_at = max(time.monotonic(), relay.next_upstream_at)
                        relay.next_upstream_at = send_at + relay.min_request_interval_ms / 1000
                        record = {"attempt": len(relay.attempts) + 1,
                                  "input_bytes": len(body), "output_cap": cap,
                                  "input_tokens_reservation": input_reservation,
                                  "retry_of_unknown_408": retrying_same_408,
                                  "state": "unknown", "usage": None,
                                  "response_terminal_event": None,
                                  "response_completed": False}
                        relay.attempts.append(record)
                        relay.input_tokens_reserved += input_reservation
                        relay.output_tokens_reserved += cap
                        relay.active_calls += 1
                if stop_reason is not None:
                    # A usage-unknown freeze cannot become healthy merely by
                    # retrying this same trial. Distinguish it from a
                    # provider's 429 without exposing any upstream body.
                    self.reject(423 if stop_reason == "usage_unknown" else 429,
                                stop_reason)
                    return
                sent = False
                max_sse_line_bytes_seen = 0
                try:
                    if relay.min_request_interval_ms:
                        time.sleep(max(0.0, send_at - time.monotonic()))
                    headers = {"Content-Type": "application/json"}
                    if relay.credential:
                        headers["Authorization"] = "Bearer " + relay.credential
                    upstream = urllib.request.Request(
                        relay.upstream, data=body, headers=headers,
                    )
                    opener = urllib.request.build_opener(
                        NoRedirect(),
                        # Local OAuth traffic must stay on loopback even if the
                        # runner process has an HTTP proxy configured.
                        urllib.request.ProxyHandler({}) if not relay.credential
                        else urllib.request.ProxyHandler(),
                    )
                    with opener.open(upstream, timeout=relay.upstream_timeout_secs) as response:
                        self.send_response(response.status)
                        self.send_header("Content-Type", response.headers.get(
                            "Content-Type", "text/event-stream"))
                        self.end_headers()
                        sent = True
                        # SSE parsing only retains a bounded unfinished line and usage.
                        pending = b""
                        buffered_lines = [] if relay.repair_missing_tool_call_ids else None
                        total = 0
                        while chunk := response.read1(16384):
                            total += len(chunk)
                            if total > 8 * 1024 * 1024:
                                raise ValueError("response bound exceeded")
                            if not relay.api_safe_tool_names:
                                self.wfile.write(chunk)
                                self.wfile.flush()
                            pending += chunk
                            lines = pending.split(b"\n")
                            pending = lines.pop()
                            max_sse_line_bytes_seen = max(
                                max_sse_line_bytes_seen, len(pending),
                            )
                            if len(pending) > 1024 * 1024:
                                raise ValueError("SSE line bound exceeded")
                            for line in lines:
                                max_sse_line_bytes_seen = max(
                                    max_sse_line_bytes_seen, len(line),
                                )
                                if len(line) > 1024 * 1024:
                                    raise ValueError("SSE line bound exceeded")
                                if relay.api_safe_tool_names:
                                    if buffered_lines is not None:
                                        buffered_lines.append(line)
                                    else:
                                        self.wfile.write(_rewrite_sse_line(
                                            line, aliases, api_protocol=relay.api_protocol
                                        ) + b"\n")
                                        self.wfile.flush()
                                if line.startswith(b"data: "):
                                    try:
                                        event = json.loads(line[6:])
                                        if isinstance(event, dict):
                                            usage = _usage_from_event(event, relay.api_protocol)
                                            if usage is not None:
                                                record["usage"] = usage
                                            terminal_event = _record_response_terminal_event(
                                                record, event,
                                            )
                                            if terminal_event == "response.completed":
                                                response_meta = event.get("response")
                                                if isinstance(response_meta, dict):
                                                    access_programs = response_meta.get("access_programs")
                                                    if isinstance(access_programs, dict):
                                                        selected = access_programs.get("cyber")
                                                        if isinstance(selected, str) and selected in {
                                                            "standard", "daybreak_blue", "daybreak_red",
                                                        }:
                                                            record["observed_cyber_program"] = selected
                                    except ValueError:
                                        pass
                        if buffered_lines is not None:
                            if pending:
                                buffered_lines.append(pending)
                                if pending.startswith(b"data: "):
                                    try:
                                        event = json.loads(pending[6:])
                                        if isinstance(event, dict):
                                            usage = _usage_from_event(event, relay.api_protocol)
                                            if usage is not None:
                                                record["usage"] = usage
                                            terminal_event = _record_response_terminal_event(
                                                record, event,
                                            )
                                            if terminal_event == "response.completed":
                                                response_meta = event.get("response")
                                                if isinstance(response_meta, dict):
                                                    access_programs = response_meta.get("access_programs")
                                                    if isinstance(access_programs, dict):
                                                        selected = access_programs.get("cyber")
                                                        if isinstance(selected, str) and selected in {
                                                            "standard", "daybreak_blue", "daybreak_red",
                                                        }:
                                                            record["observed_cyber_program"] = selected
                                    except ValueError:
                                        pass
                            buffered_lines, count = _repair_missing_tool_ids(buffered_lines)
                            record["repaired_tool_call_ids"] = count
                            output_lines = [_rewrite_sse_line(
                                                line, aliases, api_protocol=relay.api_protocol
                                            ) + b"\n"
                                            for line in buffered_lines]
                            if sum(map(len, output_lines)) > 8 * 1024 * 1024:
                                raise ValueError("rewritten response bound exceeded")
                            for line in output_lines:
                                self.wfile.write(line)
                            self.wfile.flush()
                        usage = record["usage"]
                        input_tokens = usage.get("prompt_tokens") if isinstance(usage, dict) else None
                        output_tokens = usage.get("completion_tokens") if isinstance(usage, dict) else None
                        cached_input_tokens = (
                            usage.get("cached_input_tokens", 0)
                            if isinstance(usage, dict) else None
                        )
                        if (type(input_tokens) is int and input_tokens >= 0
                                and type(output_tokens) is int and output_tokens >= 0
                                and type(cached_input_tokens) is int
                                and 0 <= cached_input_tokens <= input_tokens):
                            with relay.lock:
                                relay.input_tokens_reserved -= input_reservation
                                relay.output_tokens_reserved -= cap
                                relay.input_tokens_committed += input_tokens
                                relay.cached_input_tokens_committed += cached_input_tokens
                                relay.output_tokens_committed += output_tokens
                                if record.get("retry_of_unknown_408"):
                                    relay.retryable_408_recovered = True
                                relay.budget_exceeded = (
                                    (relay.max_input_tokens_total is not None
                                     and relay.input_tokens_committed > relay.max_input_tokens_total)
                                    or (relay.max_output_tokens_total is not None
                                        and relay.output_tokens_committed > relay.max_output_tokens_total)
                                    or (relay.max_peak_miss_usd is not None
                                        and (relay.input_price_milli_usd_per_million
                                             * relay.input_tokens_committed
                                             + relay.output_price_milli_usd_per_million
                                             * relay.output_tokens_committed)
                                        > relay.max_peak_miss_usd * 1_000_000_000)
                                )
                            record["state"] = "completed"
                            record["input_tokens"] = input_tokens
                            record["cached_input_tokens"] = cached_input_tokens
                            record["output_tokens"] = output_tokens
                            record["estimated_cost_nano_usd"] = (
                                (input_tokens - cached_input_tokens)
                                * relay.input_price_milli_usd_per_million
                                + cached_input_tokens
                                * relay.cached_input_price_milli_usd_per_million
                                + output_tokens
                                * relay.output_price_milli_usd_per_million
                            )
                            record["peak_miss_cost_nano_usd"] = (
                                input_tokens * relay.input_price_milli_usd_per_million
                                + output_tokens * relay.output_price_milli_usd_per_million
                            )
                        else:
                            with relay.lock:
                                relay.budget_unknown = True
                                # A recovered 408 only permits subsequent
                                # settled calls; a new usage gap freezes the
                                # trial even though the earlier retry worked.
                                relay.non_retryable_unknown = True
                            record["state"] = "usage_unknown"
                except Exception as error:
                    # Do not serialize exception text: a provider may reflect secrets.
                    record["error_type"] = type(error).__name__
                    local_code = _local_stream_error_code(error)
                    if local_code is not None:
                        record["local_error_code"] = local_code
                    if sent:
                        record["response_bytes_seen"] = total
                        record["max_sse_line_bytes_seen"] = max_sse_line_bytes_seen
                    if isinstance(error, http.client.IncompleteRead):
                        partial = error.partial or b""
                        record["incomplete_read_bytes"] = len(partial)
                        record["incomplete_partial_has_usage"] = b'"usage"' in partial
                        record["incomplete_partial_has_done"] = b"data: [DONE]" in partial
                        record["usage_seen_before_incomplete_read"] = (
                            isinstance(record.get("usage"), dict)
                        )
                    retryable_408 = False
                    if isinstance(error, urllib.error.HTTPError):
                        record["upstream_status_code"] = error.code
                        if error.code == 408 and not sent:
                            error.close()
                            with relay.lock:
                                if (relay.retryable_408_retries_used
                                        < relay.max_retryable_408_retries):
                                    relay.retryable_408_retries_used += 1
                                    relay.retryable_408_retry_hash = body_digest
                                    record["retryable_upstream_408"] = True
                                    retryable_408 = True
                    with relay.lock:
                        relay.budget_unknown = True
                        if not retryable_408:
                            relay.non_retryable_unknown = True
                    if not sent:
                        self.reject(
                            503 if retryable_408 else
                            423 if record.get("retry_of_unknown_408")
                            or isinstance(error, urllib.error.HTTPError)
                            and error.code == 408 else 502,
                            "retryable_upstream_408" if retryable_408 else
                            "usage_unknown" if relay.non_retryable_unknown else None,
                        )
                finally:
                    with relay.idle:
                        relay.active_calls -= 1
                        relay.idle.notify_all()

        self.server = ThreadingHTTPServer((bind, 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        address, port = self.server.server_address
        return f"http://{address}:{port}/v1"

    def close(self):
        with self.lock:
            self.closed = True
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        with self.idle:
            settled = self.idle.wait_for(lambda: self.active_calls == 0, timeout=5)
            if not settled:
                self._mark_unsettled_calls_locked()

    def _mark_unsettled_calls_locked(self):
        """Freeze any admitted request still in flight when the relay closes.

        Caller must hold ``self.lock``.  These requests may have reached the
        provider, so retain their reservations and prevent a receipt from
        claiming usage is settled.
        """
        if self.active_calls <= 0:
            return
        self.budget_unknown = True
        self.non_retryable_unknown = True
        for attempt in self.attempts:
            if attempt.get("state") == "unknown":
                attempt["state"] = "usage_unknown"
                attempt.setdefault("error_type", "ActiveCallAtClose")
                attempt.setdefault("local_error_code", "active_call_at_close")
                attempt["active_at_close"] = True

    def save_receipt(self, path: Path):
        with self.lock:
            self._mark_unsettled_calls_locked()
            estimated_cost_nano_usd = (
                (self.input_tokens_committed - self.cached_input_tokens_committed)
                * self.input_price_milli_usd_per_million
                + self.cached_input_tokens_committed
                * self.cached_input_price_milli_usd_per_million
                + self.output_price_milli_usd_per_million * self.output_tokens_committed
            )
            peak_miss_cost_nano_usd = (
                self.input_price_milli_usd_per_million * self.input_tokens_committed
                + self.output_price_milli_usd_per_million * self.output_tokens_committed
            )
            receipt = {
                "max_requests": self.max_requests,
                "max_body_bytes": self.max_body_bytes,
                "max_output_tokens": self.max_output_tokens,
                "max_input_tokens_total": self.max_input_tokens_total,
                "max_output_tokens_total": self.max_output_tokens_total,
                "max_peak_miss_usd": self.max_peak_miss_usd,
                "input_price_milli_usd_per_million": self.input_price_milli_usd_per_million,
                "cached_input_price_milli_usd_per_million": self.cached_input_price_milli_usd_per_million,
                "cached_input_price_is_assumed": self.cached_input_price_is_assumed,
                "output_price_milli_usd_per_million": self.output_price_milli_usd_per_million,
                "estimated_cost_nano_usd": estimated_cost_nano_usd,
                "peak_miss_cost_nano_usd": peak_miss_cost_nano_usd,
                "api_safe_tool_names": self.api_safe_tool_names,
                "prefer_max_completion_tokens": self.prefer_max_completion_tokens,
                "omit_stream_options": self.omit_stream_options,
                "min_request_interval_ms": self.min_request_interval_ms,
                "upstream_timeout_secs": self.upstream_timeout_secs,
                "repair_missing_tool_call_ids": self.repair_missing_tool_call_ids,
                "api_protocol": self.api_protocol,
                "standard_cyber_safeguards": self.standard_cyber_safeguards,
                "single_file_edit_patch": self.single_file_edit_patch,
                "edit_patch_files_max_per_call": 1 if self.single_file_edit_patch else 16,
                "input_tokens_reserved": self.input_tokens_reserved,
                "output_tokens_reserved": self.output_tokens_reserved,
                "input_tokens_committed": self.input_tokens_committed,
                "cached_input_tokens_committed": self.cached_input_tokens_committed,
                "output_tokens_committed": self.output_tokens_committed,
                "budget_unknown": self.budget_unknown,
                "budget_exceeded": self.budget_exceeded,
                "max_retryable_408_retries": self.max_retryable_408_retries,
                "retryable_408_retries_used": self.retryable_408_retries_used,
                "retryable_408_retry_pending": self.retryable_408_retry_hash is not None,
                "retryable_408_recovered": self.retryable_408_recovered,
                "non_retryable_unknown": self.non_retryable_unknown,
                "active_calls_after_close": self.active_calls,
                "rejections": dict(self.rejections),
                "attempts": copy.deepcopy(self.attempts),
            }
        path.write_text(json.dumps(receipt, indent=2), encoding="utf-8")
